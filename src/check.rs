use std::collections::BTreeSet;

use crate::model::{Origin, Project};
use crate::sources::SourceKind;

/// How much a finding asks of a reader.
///
/// Two levels, not five. A linter that grades its own output finely spends the
/// reader's attention arguing with it; the only question worth answering is whether
/// they have to do something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Weight {
    /// Something is wrong and a reader should change it.
    Problem,
    /// Worth knowing, but nothing here is broken.
    Note,
}

/// One thing envwire is willing to say about a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub weight: Weight,
    /// What is wrong, in the reader's terms. Never carries a value read from a
    /// `.env`: see `Value::disclosure`.
    pub what: String,
    /// Where to go to change it.
    pub at: Origin,
    /// Why this might be fine anyway, when it might.
    pub because: Option<String>,
}

/// What an example file promises and a developer's own files do not have, and back.
///
/// Files only, never containers. A key in `.env.local` really is present for whoever
/// is running the tool, even though Compose never opens that file, and an example
/// file promises regardless of whether any service reads it.
pub fn documented(project: &Project) -> Vec<Finding> {
    let held: BTreeSet<&str> = keys(project, SourceKind::Env).collect();
    if held.is_empty() && !project.files.iter().any(|f| f.kind == SourceKind::Env) {
        // A project with only an example is unconfigured, not drifting. Saying every
        // promised key is missing would be true and useless.
        return Vec::new();
    }
    let promised: BTreeSet<&str> = keys(project, SourceKind::Example).collect();

    let mut findings = Vec::new();

    for file in project
        .files
        .iter()
        .filter(|f| f.kind == SourceKind::Example)
    {
        for setting in &file.settings {
            if held.contains(setting.key.as_str()) {
                continue;
            }
            // A use that carries its own default still starts without the variable.
            // In one real project 18 of 24 missing keys were defaulted in Compose,
            // and calling all 24 a problem would have buried the six that were.
            let uses: Vec<bool> = project
                .references
                .iter()
                .filter(|r| r.name == setting.key)
                .map(|r| r.defaulted)
                .collect();
            let covered = !uses.is_empty() && uses.iter().all(|d| *d);

            findings.push(Finding {
                weight: if covered {
                    Weight::Note
                } else {
                    Weight::Problem
                },
                what: format!("{} is promised here but not set anywhere", setting.key),
                at: Origin::Line {
                    path: file.path.clone(),
                    line: setting.line,
                },
                because: covered
                    .then(|| "every use in the Compose file carries a default".to_string()),
            });
        }
    }

    for file in project.files.iter().filter(|f| f.kind == SourceKind::Env) {
        for setting in &file.settings {
            if promised.contains(setting.key.as_str()) || promised.is_empty() {
                continue;
            }
            findings.push(Finding {
                weight: Weight::Note,
                what: format!(
                    "{} is set here but no example file mentions it",
                    setting.key
                ),
                at: Origin::Line {
                    path: file.path.clone(),
                    line: setting.line,
                },
                because: None,
            });
        }
    }

    findings
}

fn keys(project: &Project, kind: SourceKind) -> impl Iterator<Item = &str> {
    project
        .files
        .iter()
        .filter(move |file| file.kind == kind)
        .flat_map(|file| file.settings.iter())
        .map(|setting| setting.key.as_str())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::{model, sources};

    fn findings(files: &[(&str, &str)]) -> (TempDir, Vec<Finding>) {
        let dir = tempdir().unwrap();
        for (name, body) in files {
            fs::write(dir.path().join(name), body).unwrap();
        }
        let project = model::read(&sources::discover(dir.path())).unwrap();
        let found = documented(&project);
        (dir, found)
    }

    fn said(found: &[Finding]) -> Vec<(Weight, String)> {
        found.iter().map(|f| (f.weight, f.what.clone())).collect()
    }

    #[test]
    fn a_project_that_matches_its_example_says_nothing() {
        let (_dir, found) = findings(&[(".env", "A=1\nB=2\n"), (".env.example", "A=\nB=\n")]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_promised_key_nobody_sets_is_a_problem() {
        let (_dir, found) = findings(&[(".env", "A=1\n"), (".env.example", "A=\nMISSING=\n")]);
        assert_eq!(
            said(&found),
            [(
                Weight::Problem,
                "MISSING is promised here but not set anywhere".to_string()
            )]
        );
        assert!(matches!(found[0].at, Origin::Line { line: 2, .. }));
    }

    #[test]
    fn a_promised_key_compose_always_defaults_is_only_a_note() {
        // The service starts without it, so calling it a problem buries the ones that
        // really are. Taken from a real project where 18 of 24 looked like this.
        let (_dir, found) = findings(&[
            (".env", "A=1\n"),
            (".env.example", "A=\nSMTP_HOST=\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    environment:\n      SMTP_HOST: ${SMTP_HOST:-mail}\n",
            ),
        ]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].weight, Weight::Note);
        assert!(found[0].because.is_some());
    }

    #[test]
    fn one_use_without_a_default_is_enough_to_make_it_a_problem() {
        let (_dir, found) = findings(&[
            (".env", "A=1\n"),
            (".env.example", "A=\nHOST=\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    environment:\n      X: ${HOST:-safe}\n      Y: ${HOST}\n",
            ),
        ]);
        assert_eq!(found[0].weight, Weight::Problem);
    }

    #[test]
    fn a_key_only_the_local_file_holds_still_counts_as_held() {
        // Compose never opens `.env.local`, but the developer running envwire has it.
        let (_dir, found) = findings(&[
            (".env", "A=1\n"),
            (".env.local", "B=2\n"),
            (".env.example", "A=\nB=\n"),
        ]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_key_no_example_mentions_is_a_note() {
        let (_dir, found) =
            findings(&[(".env", "A=1\nUNDOCUMENTED=2\n"), (".env.example", "A=\n")]);
        assert_eq!(
            said(&found),
            [(
                Weight::Note,
                "UNDOCUMENTED is set here but no example file mentions it".to_string()
            )]
        );
    }

    #[test]
    fn a_project_with_no_example_is_not_undocumented() {
        // There is nothing to disagree with, and saying every key is undocumented
        // would fire on every project that never wrote an example.
        let (_dir, found) = findings(&[(".env", "A=1\nB=2\n")]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_project_with_only_an_example_is_unconfigured_not_drifting() {
        let (_dir, found) = findings(&[(".env.example", "A=\nB=\n")]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_pass_through_line_counts_as_holding_the_key() {
        // `BARE` with no delimiter asks for the host's value; the project has said
        // the key exists, which is what the example is asking about.
        let (_dir, found) = findings(&[(".env", "BARE\n"), (".env.example", "BARE=\n")]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn no_finding_ever_carries_a_value() {
        let (_dir, found) = findings(&[
            (".env", "SECRET=hunter2-should-never-appear\n"),
            (".env.example", "OTHER=\n"),
        ]);
        for finding in &found {
            assert!(
                !finding.what.contains("hunter2"),
                "leaked: {}",
                finding.what
            );
        }
    }
}
