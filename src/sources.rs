use std::path::{Path, PathBuf};

/// The kind of claim a file makes about the environment.
///
/// The kind is what lets envwire compare files later: an example promises which
/// variables exist, a local file says what they are set to, and Compose decides
/// what a container actually receives. Disagreement only means something once you
/// know which of those a value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// Values a developer runs with.
    Env,
    /// Values the project promises a newcomer will need.
    Example,
    /// Values Compose hands to a service.
    Compose,
}

impl SourceKind {
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Env => "env",
            SourceKind::Example => "example",
            SourceKind::Compose => "compose",
        }
    }
}

/// A file envwire found and knows how to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub kind: SourceKind,
    pub path: PathBuf,
}

/// The filenames envwire looks for, in the order it reports them.
const KNOWN: &[(&str, SourceKind)] = &[
    (".env", SourceKind::Env),
    (".env.local", SourceKind::Env),
    (".env.example", SourceKind::Example),
    (".env.sample", SourceKind::Example),
    (".env.template", SourceKind::Example),
    ("compose.yaml", SourceKind::Compose),
    ("compose.yml", SourceKind::Compose),
    ("docker-compose.yaml", SourceKind::Compose),
    ("docker-compose.yml", SourceKind::Compose),
];

/// Look for env sources sitting directly in `dir`.
///
/// Only the top level. A project states its environment at its root, and walking
/// deeper would sweep up the fixtures of every dependency it vendors, then report
/// a stranger's test data as your drift.
pub fn discover(dir: &Path) -> Vec<Source> {
    KNOWN
        .iter()
        .filter_map(|&(name, kind)| {
            let path = dir.join(name);
            path.is_file().then_some(Source { kind, path })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    fn names(sources: &[Source]) -> Vec<String> {
        sources
            .iter()
            .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn an_empty_directory_has_nothing_to_report() {
        let dir = tempdir().unwrap();
        assert!(discover(dir.path()).is_empty());
    }

    #[test]
    fn each_file_is_reported_under_the_claim_it_makes() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".env"), "").unwrap();
        fs::write(dir.path().join(".env.example"), "").unwrap();
        fs::write(dir.path().join("docker-compose.yml"), "").unwrap();

        let found = discover(dir.path());
        let kinds: Vec<_> = found.iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            [SourceKind::Env, SourceKind::Example, SourceKind::Compose]
        );
    }

    #[test]
    fn unknown_files_are_left_alone() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "").unwrap();
        fs::write(dir.path().join(".envrc"), "").unwrap();
        assert!(discover(dir.path()).is_empty());
    }

    #[test]
    fn a_directory_named_like_a_source_is_not_one() {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join(".env")).unwrap();
        assert!(discover(dir.path()).is_empty());
    }

    #[test]
    fn nested_sources_stay_out_of_the_report() {
        let dir = tempdir().unwrap();
        let vendored = dir.path().join("vendor").join("some-crate");
        fs::create_dir_all(&vendored).unwrap();
        fs::write(vendored.join(".env"), "").unwrap();
        assert!(discover(dir.path()).is_empty());
    }

    #[test]
    fn the_report_keeps_a_stable_order() {
        let dir = tempdir().unwrap();
        for name in ["docker-compose.yml", ".env.example", ".env"] {
            fs::write(dir.path().join(name), "").unwrap();
        }
        assert_eq!(
            names(&discover(dir.path())),
            [".env", ".env.example", "docker-compose.yml"]
        );
    }
}
