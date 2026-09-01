use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::dotenv::{self, Malformed};
use crate::error::{Error, Result};
use crate::sources::{Source, SourceKind};
use crate::template::{Template, Value};

/// Where something is written.
///
/// A `.env` finding points at a line, because a reader can open it there. A Compose
/// finding cannot: yaml-rust2 exposes no position markers, so the service name is as
/// precise as it is honest to be. That second shape arrives with the services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Line { path: PathBuf, line: usize },
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Line { path, line } => write!(f, "{}:{line}", path.display()),
        }
    }
}

/// A value and where to go to change it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub value: Value,
    pub origin: Origin,
}

/// One line of a `.env`-shaped file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Setting {
    pub key: String,
    /// `None` for a bare `NAME` with no delimiter: a request to pass the host's
    /// value through, not a setting, and never something to report a value for.
    pub value: Option<Value>,
    pub line: usize,
}

/// One `.env`-shaped file, kept whole.
#[derive(Debug, Clone)]
pub struct EnvFile {
    pub path: PathBuf,
    /// File order, duplicates kept: which of two assignments wins is a finding, and
    /// collapsing them would throw away the evidence that there were two.
    pub settings: Vec<Setting>,
    /// What stayed unreadable after Docker's grammar was tried too.
    pub malformed: Vec<Malformed>,
}

/// The variables Compose holds while expanding `${...}` in the YAML text.
///
/// Only the project `.env` goes in here. A service's `env_file:` never takes part in
/// interpolation, not even when it names `.env` itself -- two projects, one with
/// `env_file: .env` and one without, interpolate identically, and only what lands in
/// the container differs. `.env.local` is a framework convention Compose has never
/// opened; both facts were checked against `docker compose config`, not read.
///
/// The map is private and there is deliberately no way to iterate it. A list of
/// "the variables this project has" is what makes every `.env` key look like it
/// belongs in every container, and that one shortcut would produce more wrong
/// findings than every other mistake here put together.
///
/// Nothing from the process environment is ever read: a linter whose findings change
/// with the shell that ran it cannot be trusted in CI.
#[derive(Debug, Default)]
pub struct Interpolation {
    values: BTreeMap<String, Bound>,
}

impl Interpolation {
    pub fn get(&self, name: &str) -> Option<&Bound> {
        self.values.get(name)
    }
}

/// A `$VAR` or `${...}` somewhere in the Compose file's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub name: String,
    pub origin: Origin,
}

/// Everything the checks are allowed to reason from.
#[derive(Debug)]
pub struct Project {
    pub files: Vec<EnvFile>,
    pub interpolation: Interpolation,
    /// The Compose file, when there is one. A question about what containers receive
    /// has no answer in a project without it.
    pub compose: Option<PathBuf>,
    /// Every reference in the raw Compose text, with its line. Duplicates kept.
    pub references: Vec<Reference>,
}

/// Read a project into the one shape every check reasons from.
///
/// The assumption this rests on, stated rather than asserted as Compose semantics:
/// the ordinary invocation, where the project directory is the directory envwire was
/// pointed at. `--env-file`, `COMPOSE_ENV_FILES` and `COMPOSE_DISABLE_ENV_FILE` each
/// replace the interpolation file and are invisible from here.
pub fn read(sources: &[Source]) -> Result<Project> {
    let mut files = Vec::new();
    for source in sources {
        if source.kind == SourceKind::Compose {
            continue;
        }
        let doc = dotenv::read(&source.path)?;
        let (settings, malformed) = settings_of(doc, source.kind);
        files.push(EnvFile {
            path: source.path.clone(),
            settings,
            malformed,
        });
    }

    let interpolation = interpolation_of(&files);

    let compose = sources
        .iter()
        .find(|s| s.kind == SourceKind::Compose)
        .map(|s| s.path.clone());
    let references = match &compose {
        Some(path) => {
            let raw = fs::read_to_string(path).map_err(|source| Error::Read {
                path: path.clone(),
                source,
            })?;
            scan(&raw, path)
        }
        None => Vec::new(),
    };

    Ok(Project {
        files,
        interpolation,
        compose,
        references,
    })
}

/// What one file states, with the lines Docker would still accept recovered.
///
/// dotenv.rs is written for the strict `KEY=value` grammar every framework loader
/// uses, and files two shapes under `malformed` that Docker's env-file grammar reads:
/// a bare `NAME`, which asks for the host's value, and `NAME: value`, whose delimiter
/// is a colon. Both were confirmed against `docker compose config`.
///
/// Only for files a developer runs with. An example file is not an env file -- Docker
/// never reads one -- and recovering a line of prose out of one would invent a key the
/// project is then reported as failing to provide.
fn settings_of(doc: dotenv::Document, kind: SourceKind) -> (Vec<Setting>, Vec<Malformed>) {
    let mut settings: Vec<Setting> = doc
        .entries
        .iter()
        .map(|entry| Setting {
            key: entry.key.clone(),
            value: Some(Value::stated(&entry.value)),
            line: entry.line,
        })
        .collect();

    let mut malformed = Vec::new();
    for bad in doc.malformed {
        if kind != SourceKind::Env {
            malformed.push(bad);
            continue;
        }
        if dotenv::is_name(&bad.text) {
            settings.push(Setting {
                key: bad.text.clone(),
                value: None,
                line: bad.line,
            });
            continue;
        }
        match colon_form(&bad.text) {
            Some(setting) => settings.push(Setting {
                line: bad.line,
                ..setting
            }),
            None => malformed.push(bad),
        }
    }

    settings.sort_by_key(|setting| setting.line);
    (settings, malformed)
}

/// Read `NAME: value` by swapping the delimiter and letting dotenv.rs do the rest.
///
/// Reusing its grammar rather than repeating it keeps quoting, escapes and trailing
/// comments behaving the same way on both spellings.
fn colon_form(text: &str) -> Option<Setting> {
    let (name, rest) = text.split_once(':')?;
    let name = name.trim();
    if !dotenv::is_name(name) {
        return None;
    }
    let entry = dotenv::parse(&format!("{name}={rest}")).entries.pop()?;
    Some(Setting {
        key: entry.key,
        value: Some(Value::stated(&entry.value)),
        line: 0,
    })
}

/// Build what Compose would expand `${...}` from.
fn interpolation_of(files: &[EnvFile]) -> Interpolation {
    let mut interpolation = Interpolation::default();
    let Some(file) = files
        .iter()
        .find(|f| f.path.file_name().and_then(|n| n.to_str()) == Some(".env"))
    else {
        return interpolation;
    };

    for setting in &file.settings {
        // A pass-through request defines nothing, so it cannot answer a `${...}`.
        let Some(value) = &setting.value else {
            continue;
        };
        // Last wins, the way a shell sourcing the file would end up.
        interpolation.values.insert(
            setting.key.clone(),
            Bound {
                value: value.clone(),
                origin: Origin::Line {
                    path: file.path.clone(),
                    line: setting.line,
                },
            },
        );
    }
    interpolation
}

/// Every variable the raw Compose text names, with the line that names it.
///
/// Deliberately separate from the parsed model: compose.rs captures only
/// `environment` and `env_file`, so `image: app:${TAG}` and `ports: ["${PORT}:80"]`
/// never reach it, and a usage check built on the model alone would call them unused.
fn scan(raw: &str, path: &Path) -> Vec<Reference> {
    let mut references = Vec::new();
    for (index, line) in raw.lines().enumerate() {
        let mut names = Vec::new();
        Template::parse(line).names(&mut names);
        for name in names {
            references.push(Reference {
                name,
                origin: Origin::Line {
                    path: path.to_path_buf(),
                    line: index + 1,
                },
            });
        }
    }
    references
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::{TempDir, tempdir};

    use super::*;

    /// Write a project and read it back the way the binary would.
    fn project(files: &[(&str, &str)]) -> (TempDir, Project) {
        let dir = tempdir().unwrap();
        for (name, body) in files {
            fs::write(dir.path().join(name), body).unwrap();
        }
        let sources = crate::sources::discover(dir.path());
        let project = read(&sources).unwrap();
        (dir, project)
    }

    fn settings(project: &Project, name: &str) -> Vec<(String, Option<Value>)> {
        project
            .files
            .iter()
            .find(|f| f.path.file_name().and_then(|n| n.to_str()) == Some(name))
            .expect("file was read")
            .settings
            .iter()
            .map(|s| (s.key.clone(), s.value.clone()))
            .collect()
    }

    fn literal(text: &str) -> Option<Value> {
        Some(Value::Literal(text.to_string()))
    }

    #[test]
    fn a_plain_env_file_becomes_its_settings() {
        let (_dir, project) = project(&[(".env", "HOST=redis\nPORT=6379\n")]);
        assert_eq!(
            settings(&project, ".env"),
            [
                ("HOST".to_string(), literal("redis")),
                ("PORT".to_string(), literal("6379"))
            ]
        );
    }

    #[test]
    fn a_value_naming_a_variable_is_not_stated() {
        let (_dir, project) = project(&[(".env", "A=plain\nB=${A}/more\n")]);
        assert_eq!(
            settings(&project, ".env"),
            [
                ("A".to_string(), literal("plain")),
                ("B".to_string(), Some(Value::Unknown))
            ]
        );
    }

    #[test]
    fn docker_shapes_are_recovered_in_a_file_a_developer_runs_with() {
        // Both confirmed against `docker compose config`: a bare name asks for the
        // host's value, and a colon is a delimiter Docker accepts.
        let (_dir, project) = project(&[(".env", "PASS_THROUGH\nCOLON: works\nGOOD=yes\n")]);
        assert_eq!(
            settings(&project, ".env"),
            [
                ("PASS_THROUGH".to_string(), None),
                ("COLON".to_string(), literal("works")),
                ("GOOD".to_string(), literal("yes")),
            ]
        );
        let file = &project.files[0];
        assert!(file.malformed.is_empty(), "{:?}", file.malformed);
    }

    #[test]
    fn an_example_file_gets_no_such_forgiveness() {
        // Docker never reads an example file, so a line of prose in one is a typo,
        // not a setting. Recovering it would invent a key the project is then
        // reported as failing to provide.
        let (_dir, project) =
            project(&[(".env.example", "PASS_THROUGH\nsee the readme: really\n")]);
        let file = &project.files[0];
        assert!(file.settings.is_empty(), "{:?}", file.settings);
        assert_eq!(file.malformed.len(), 2);
    }

    #[test]
    fn a_line_no_grammar_accepts_stays_unreadable() {
        let (_dir, project) = project(&[(".env", "just some words\nGOOD=yes\n")]);
        assert_eq!(
            settings(&project, ".env"),
            [("GOOD".to_string(), literal("yes"))]
        );
        assert_eq!(project.files[0].malformed.len(), 1);
    }

    #[test]
    fn recovered_lines_are_reported_in_file_order() {
        let (_dir, project) = project(&[(".env", "A=1\nBARE\nB=2\n")]);
        let lines: Vec<usize> = project.files[0].settings.iter().map(|s| s.line).collect();
        assert_eq!(lines, [1, 2, 3]);
    }

    #[test]
    fn only_the_project_env_answers_an_interpolation() {
        let (_dir, project) = project(&[
            (".env", "SHARED=from-env\n"),
            (".env.local", "LOCAL_ONLY=x\nSHARED=from-local\n"),
            (".env.example", "EXAMPLE_ONLY=y\n"),
        ]);
        assert_eq!(
            project.interpolation.get("SHARED").map(|b| b.value.clone()),
            Some(Value::Literal("from-env".to_string()))
        );
        // Compose has never opened either of these.
        assert!(project.interpolation.get("LOCAL_ONLY").is_none());
        assert!(project.interpolation.get("EXAMPLE_ONLY").is_none());
    }

    #[test]
    fn the_last_assignment_is_the_one_a_shell_would_keep() {
        let (_dir, project) = project(&[(".env", "KEY=first\nKEY=second\n")]);
        let bound = project.interpolation.get("KEY").unwrap();
        assert_eq!(bound.value, Value::Literal("second".to_string()));
        assert!(matches!(bound.origin, Origin::Line { line: 2, .. }));
    }

    #[test]
    fn a_pass_through_answers_no_interpolation() {
        let (_dir, project) = project(&[(".env", "BARE\n")]);
        assert!(project.interpolation.get("BARE").is_none());
    }

    #[test]
    fn an_unreadable_value_is_carried_as_unknown_not_dropped() {
        // Dropping it would let a reference to it resolve to nothing at all, which
        // reads as "unset" when the truth is "set to something we cannot see".
        let (_dir, project) = project(&[(".env", "A=${B}\n")]);
        assert_eq!(
            project.interpolation.get("A").map(|b| b.value.clone()),
            Some(Value::Unknown)
        );
    }

    #[test]
    fn references_are_found_wherever_they_sit_in_the_file() {
        let (_dir, project) = project(&[(
            "docker-compose.yml",
            "services:\n  api:\n    image: app:${TAG}\n    ports:\n      - \"${PORT}:80\"\n    environment:\n      HOST: ${DB_HOST:-db}\n",
        )]);
        let found: Vec<(&str, &Origin)> = project
            .references
            .iter()
            .map(|r| (r.name.as_str(), &r.origin))
            .collect();
        // image: and ports: are invisible to the parsed model, which is the whole
        // reason the scan reads the raw text instead.
        assert_eq!(
            found.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            ["TAG", "PORT", "DB_HOST"]
        );
        assert!(matches!(found[0].1, Origin::Line { line: 3, .. }));
        assert!(matches!(found[2].1, Origin::Line { line: 7, .. }));
    }

    #[test]
    fn a_project_without_compose_has_nothing_to_scan() {
        let (_dir, project) = project(&[(".env", "A=1\n")]);
        assert!(project.compose.is_none());
        assert!(project.references.is_empty());
    }

    #[test]
    fn an_origin_says_where_to_look() {
        let origin = Origin::Line {
            path: PathBuf::from("/srv/app/.env"),
            line: 12,
        };
        assert_eq!(origin.to_string(), "/srv/app/.env:12");
    }
}
