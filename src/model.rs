use std::path::PathBuf;

use crate::dotenv::{self, Malformed};
use crate::error::Result;
use crate::sources::{Source, SourceKind};
use crate::template::Value;

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

/// Everything the checks are allowed to reason from.
#[derive(Debug)]
pub struct Project {
    pub files: Vec<EnvFile>,
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
    Ok(Project { files })
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
        assert!(project.files[0].malformed.is_empty());
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
    fn both_halves_of_a_repeated_key_survive_the_read() {
        // Which assignment wins is a finding; collapsing them here would throw away
        // the evidence that there were two.
        let (_dir, project) = project(&[(".env", "KEY=first\nKEY=second\n")]);
        assert_eq!(
            settings(&project, ".env"),
            [
                ("KEY".to_string(), literal("first")),
                ("KEY".to_string(), literal("second"))
            ]
        );
    }

    #[test]
    fn a_compose_file_is_not_read_as_an_env_file() {
        let (_dir, project) = project(&[
            (".env", "A=1\n"),
            ("docker-compose.yml", "services:\n  api:\n    image: app\n"),
        ]);
        assert_eq!(project.files.len(), 1);
    }
}
