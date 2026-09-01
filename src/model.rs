use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::compose;
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
    /// The place that decided this key.
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

/// Which layer of a service's environment set a key.
///
/// Ordered on purpose, and the derived `Ord` is the precedence rule itself: a later
/// `env_file` beats an earlier one. Confirmed against `docker compose config`, per
/// key rather than per file. The layer `environment:` sets arrives with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    EnvFile(usize),
}

/// Something about a service envwire could not read, and how far up it reaches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    pub layer: Layer,
    pub what: Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Missing {
    /// An `env_file:` that is not on disk. Its contents are a guess, and a guess must
    /// silence a finding rather than be treated as an empty file.
    UnreadFile(PathBuf),
}

/// One variable a service's containers would start with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Var {
    pub key: String,
    pub bound: Bound,
    pub layer: Layer,
}

/// The environment one service's containers would start with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceEnv {
    pub name: String,
    /// Keys in the order they were first set, values as last written. Losers are
    /// dropped: reporting a value that something downstream overrides is a false
    /// positive by construction.
    pub vars: Vec<Var>,
    /// The `env_file:` list, resolved and in order, whether or not each one read.
    /// A check needs this even when none of their values survived the fold: a service
    /// reading `./config/.env.production` has a source the root `.env` knows nothing
    /// about, and calling that disagreement drift would be wrong.
    pub sources: Vec<PathBuf>,
    pub gaps: Vec<Gap>,
}

impl ServiceEnv {
    /// Whether anything envwire could not read could still overrule `var`.
    ///
    /// Exact rather than conservative: a value from a later env file survives a gap
    /// in an earlier one, because a later file wins. Only what could still overrule
    /// this key silences it.
    pub fn settled(&self, var: &Var) -> bool {
        self.gaps.iter().all(|gap| gap.layer < var.layer)
    }
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
    /// What each service would actually be handed. Empty without a Compose file.
    pub services: Vec<ServiceEnv>,
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
    let (references, services) = match &compose {
        Some(path) => {
            let raw = fs::read_to_string(path).map_err(|source| Error::Read {
                path: path.clone(),
                source,
            })?;
            let parsed = compose::read(path)?;
            let mut cache = HashMap::new();
            let services = parsed
                .services
                .iter()
                .map(|service| fold(service, path, &mut cache))
                .collect();
            (scan(&raw, path), services)
        }
        None => (Vec::new(), Vec::new()),
    };

    Ok(Project {
        files,
        interpolation,
        compose,
        references,
        services,
    })
}

/// Work out what one service's containers would start with.
///
/// The order is Docker's: every `env_file` in the order written, then `environment`
/// over the top, last writer winning per key. Nothing here consults the shell.
fn fold(
    service: &compose::Service,
    compose_path: &Path,
    cache: &mut HashMap<PathBuf, Vec<Setting>>,
) -> ServiceEnv {
    let mut env = ServiceEnv {
        name: service.name.clone(),
        vars: Vec::new(),
        sources: Vec::new(),
        gaps: Vec::new(),
    };

    // `env_file:` hangs off the Compose file's own folder -- a different anchor from
    // the project directory that locates `.env`, and getting it wrong is a
    // file-not-found on every layout where the two differ.
    let anchor = compose_path.parent().unwrap_or(Path::new("."));
    for (index, relative) in service.env_files.iter().enumerate() {
        let path = anchor.join(relative);
        env.sources.push(path.clone());

        let layer = Layer::EnvFile(index);
        let settings = match cache.get(&path) {
            Some(settings) => settings.clone(),
            None => match dotenv::read(&path) {
                // An env file Compose reads is an env file, so it gets the same
                // forgiveness a `.env` does.
                Ok(doc) => {
                    let (settings, _) = settings_of(doc, SourceKind::Env);
                    cache.insert(path.clone(), settings.clone());
                    settings
                }
                Err(_) => {
                    env.gaps.push(Gap {
                        layer,
                        what: Missing::UnreadFile(path),
                    });
                    continue;
                }
            },
        };

        for setting in settings {
            // A bare name inside an env file asks for a value envwire cannot see.
            let value = setting.value.unwrap_or(Value::Unknown);
            set(
                &mut env.vars,
                setting.key,
                Bound {
                    value,
                    origin: Origin::Line {
                        path: path.clone(),
                        line: setting.line,
                    },
                },
                layer,
            );
        }
    }

    env
}

/// Last writer wins, and the order a key was first set in is kept.
fn set(vars: &mut Vec<Var>, key: String, bound: Bound, layer: Layer) {
    match vars.iter_mut().find(|var| var.key == key) {
        Some(existing) => {
            existing.bound = bound;
            existing.layer = layer;
        }
        None => vars.push(Var { key, bound, layer }),
    }
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

    fn service<'a>(project: &'a Project, name: &str) -> &'a ServiceEnv {
        project
            .services
            .iter()
            .find(|s| s.name == name)
            .expect("service was folded")
    }

    fn var<'a>(env: &'a ServiceEnv, key: &str) -> &'a Var {
        env.vars.iter().find(|v| v.key == key).expect("key was set")
    }

    #[test]
    fn an_env_file_becomes_the_service_environment() {
        let (_dir, project) = project(&[
            ("svc.env", "HOST=db\nPORT=5432\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file: svc.env\n",
            ),
        ]);
        let api = service(&project, "api");
        let keys: Vec<&str> = api.vars.iter().map(|v| v.key.as_str()).collect();
        assert_eq!(keys, ["HOST", "PORT"]);
        assert_eq!(var(api, "HOST").bound.value, Value::Literal("db".into()));
    }

    #[test]
    fn a_later_env_file_beats_an_earlier_one() {
        // Verified against `docker compose config`: per key, in the order written.
        let (_dir, project) = project(&[
            ("a.env", "K=first\nONLY_A=kept\n"),
            ("b.env", "K=second\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file:\n      - a.env\n      - b.env\n",
            ),
        ]);
        let api = service(&project, "api");
        assert_eq!(var(api, "K").bound.value, Value::Literal("second".into()));
        assert_eq!(var(api, "K").layer, Layer::EnvFile(1));
        assert_eq!(
            var(api, "ONLY_A").bound.value,
            Value::Literal("kept".into())
        );
    }

    #[test]
    fn keys_keep_the_order_they_were_first_set_in() {
        let (_dir, project) = project(&[
            ("a.env", "FIRST=1\nSECOND=2\n"),
            ("b.env", "FIRST=overridden\nTHIRD=3\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file:\n      - a.env\n      - b.env\n",
            ),
        ]);
        let keys: Vec<&str> = service(&project, "api")
            .vars
            .iter()
            .map(|v| v.key.as_str())
            .collect();
        assert_eq!(keys, ["FIRST", "SECOND", "THIRD"]);
    }

    #[test]
    fn an_env_file_that_is_not_there_is_a_gap_not_an_empty_file() {
        let (_dir, project) = project(&[(
            "docker-compose.yml",
            "services:\n  api:\n    env_file: missing.env\n",
        )]);
        let api = service(&project, "api");
        assert_eq!(api.gaps.len(), 1);
        assert!(matches!(api.gaps[0].what, Missing::UnreadFile(_)));
        assert_eq!(api.gaps[0].layer, Layer::EnvFile(0));
        // The path is still recorded: a service reading a file envwire cannot see has
        // a source the root .env knows nothing about.
        assert_eq!(api.sources.len(), 1);
    }

    #[test]
    fn a_gap_above_a_value_silences_it() {
        let (_dir, project) = project(&[
            ("a.env", "K=v\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file:\n      - a.env\n      - missing.env\n",
            ),
        ]);
        let api = service(&project, "api");
        assert!(!api.settled(var(api, "K")));
    }

    #[test]
    fn a_gap_below_a_value_cannot_overrule_it() {
        let (_dir, project) = project(&[
            ("b.env", "K=v\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file:\n      - missing.env\n      - b.env\n",
            ),
        ]);
        let api = service(&project, "api");
        // A later file wins, so nothing unread below it can change this value.
        assert!(api.settled(var(api, "K")));
    }

    #[test]
    fn a_bare_name_in_an_env_file_is_a_value_nobody_here_can_see() {
        let (_dir, project) = project(&[
            ("svc.env", "PASS_THROUGH\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file: svc.env\n",
            ),
        ]);
        let api = service(&project, "api");
        assert_eq!(var(api, "PASS_THROUGH").bound.value, Value::Unknown);
    }

    #[test]
    fn an_env_file_hangs_off_the_compose_files_own_folder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config/svc.env"), "K=found\n").unwrap();
        std::fs::write(
            dir.path().join("docker-compose.yml"),
            "services:\n  api:\n    env_file: config/svc.env\n",
        )
        .unwrap();
        let project = read(&crate::sources::discover(dir.path())).unwrap();
        let api = service(&project, "api");
        assert_eq!(var(api, "K").bound.value, Value::Literal("found".into()));
        assert!(api.gaps.is_empty());
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
