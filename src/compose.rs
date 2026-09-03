use std::fs;
use std::path::{Path, PathBuf};

use yaml_rust2::{Yaml, YamlLoader};

use crate::error::{Error, Result};

/// How a key was written, which decides whether Compose expanded it.
///
/// Only the list form interpolates keys. In the mapping form `${WHICH}: value` is a
/// key literally named `${WHICH}` -- `docker compose config` renders it back as
/// `$${WHICH}`, which is Compose saying the text stands as typed. Reading the two
/// forms alike makes envwire declare it cannot know what the service is handed, and
/// that admission silences every finding for the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySource {
    /// `KEY: value`. Never interpolated.
    Mapping,
    /// `- KEY=value`. Compose expands the key as well as the value.
    List,
}

/// One variable a service is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub key: String,
    pub key_source: KeySource,
    /// `None` when the service takes whatever the host has, which Compose writes
    /// as a bare `- REDIS_HOST` or a `REDIS_HOST:` with nothing after it.
    pub value: Option<String>,
}

/// A file a service pulls variables from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvFileRef {
    /// As written in the file, so still relative to the Compose file's directory.
    pub path: PathBuf,
    /// Compose refuses to start when a required file is missing, so its absence is a
    /// broken project. An optional one that is absent is a resolved fact the author
    /// wrote down, and treating that as something envwire could not read invents a
    /// doubt the project does not have.
    pub required: bool,
}

/// A service and the environment the Compose file gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub name: String,
    /// Set inline, in file order. Repeats are kept; which one wins is a finding.
    pub environment: Vec<Assignment>,
    pub env_files: Vec<EnvFileRef>,
}

/// What a Compose file hands to each of its services.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Compose {
    /// In file order, so a report reads in the order the author wrote.
    pub services: Vec<Service>,
}

/// Read and parse a Compose file.
pub fn read(path: &Path) -> Result<Compose> {
    let text = fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })?;
    parse(&text).map_err(|message| Error::Yaml {
        path: path.to_path_buf(),
        message,
    })
}

/// Parse Compose YAML.
///
/// Only the parts that decide a service's environment are read. Everything else a
/// Compose file carries is somebody else's business.
pub fn parse(text: &str) -> std::result::Result<Compose, String> {
    let docs = YamlLoader::load_from_str(text).map_err(|e| e.to_string())?;
    let Some(doc) = docs.first() else {
        // An empty file is a file with no services, not a broken one.
        return Ok(Compose::default());
    };

    let mut compose = Compose::default();
    for (name, body) in fields(&doc["services"]) {
        let Some(name) = name.as_str() else { continue };
        compose.services.push(Service {
            name: name.to_string(),
            environment: field(body, "environment").map_or_else(Vec::new, environment_of),
            env_files: field(body, "env_file").map_or_else(Vec::new, env_files_of),
        });
    }

    Ok(compose)
}

/// The key that YAML mappings borrow another mapping's entries with.
const MERGE: &str = "<<";

/// A mapping's entries, with the ones it borrows through `<<` folded in.
///
/// Compose files share blocks through anchors, and the parser hands `<<: *defaults`
/// back as a key literally named `<<`. Left alone, a service that inherits its
/// environment that way looks empty, and every variable it really does receive
/// would be reported missing.
///
/// Precedence follows YAML: what a mapping states itself beats what it borrows,
/// and an earlier `<<` beats a later one.
fn fields(node: &Yaml) -> Vec<(&Yaml, &Yaml)> {
    let Some(map) = node.as_hash() else {
        return Vec::new();
    };

    let mut out: Vec<(&Yaml, &Yaml)> = Vec::new();
    let mut borrowed: Vec<&Yaml> = Vec::new();
    for (key, value) in map {
        if key.as_str() == Some(MERGE) {
            borrowed.push(value);
        } else {
            out.push((key, value));
        }
    }

    for source in borrowed {
        // `<<` takes one mapping or a list of them.
        let sources = match source {
            Yaml::Array(items) => items.iter().collect(),
            single => vec![single],
        };
        for source in sources {
            for (key, value) in fields(source) {
                let seen = out
                    .iter()
                    .any(|(other, _)| other.as_str().is_some() && other.as_str() == key.as_str());
                if !seen {
                    out.push((key, value));
                }
            }
        }
    }

    out
}

/// One field of a mapping, looked up through whatever it borrows.
fn field<'a>(node: &'a Yaml, name: &str) -> Option<&'a Yaml> {
    fields(node)
        .into_iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

/// Read an `environment:` block, which Compose accepts in two shapes.
fn environment_of(node: &Yaml) -> Vec<Assignment> {
    match node {
        // environment:
        //   KEY: value
        Yaml::Hash(_) => fields(node)
            .into_iter()
            .filter_map(|(key, value)| {
                let key = key.as_str()?;
                Some(Assignment {
                    key: key.to_string(),
                    key_source: KeySource::Mapping,
                    value: scalar(value),
                })
            })
            .collect(),

        // environment:
        //   - KEY=value
        //   - KEY
        Yaml::Array(items) => items
            .iter()
            .filter_map(|item| {
                let item = item.as_str()?;
                Some(match item.split_once('=') {
                    Some((key, value)) => Assignment {
                        key: key.trim().to_string(),
                        key_source: KeySource::List,
                        value: Some(value.to_string()),
                    },
                    None => Assignment {
                        key: item.trim().to_string(),
                        key_source: KeySource::List,
                        value: None,
                    },
                })
            })
            .collect(),

        _ => Vec::new(),
    }
}

/// Read an `env_file:`, which may name one file, several, or several with options.
fn env_files_of(node: &Yaml) -> Vec<EnvFileRef> {
    let required = |path: &str| EnvFileRef {
        path: PathBuf::from(path),
        required: true,
    };
    match node {
        Yaml::String(one) => vec![required(one)],
        Yaml::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Yaml::String(path) => Some(required(path)),
                // The long form: `- path: .env.local` with `required:` beside it,
                // which defaults to true when the author leaves it out.
                Yaml::Hash(_) => item["path"].as_str().map(|path| EnvFileRef {
                    path: PathBuf::from(path),
                    required: item["required"].as_bool().unwrap_or(true),
                }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The string a container would receive for a YAML scalar.
///
/// Compose hands every value to the process as text, so `true` arrives as "true"
/// and `5432` as "5432". A float keeps the digits as written, so `1.10` does not
/// quietly become `1.1`.
///
/// One value cannot survive the round trip: an integer written with a leading zero
/// loses it, because YAML reads it as a number before envwire ever sees the text.
/// Quoting it in the Compose file — `PORT: "0755"` — keeps it whole, and is what a
/// service that cares about the zero needs anyway.
fn scalar(node: &Yaml) -> Option<String> {
    match node {
        Yaml::String(s) => Some(s.clone()),
        Yaml::Boolean(b) => Some(b.to_string()),
        Yaml::Integer(i) => Some(i.to_string()),
        Yaml::Real(r) => Some(r.clone()),
        // `KEY:` with nothing after it means "pass the host's value through".
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(text: &str) -> Compose {
        parse(text).expect("should parse")
    }

    fn only_service(text: &str) -> Service {
        let mut compose = parse_ok(text);
        assert_eq!(compose.services.len(), 1);
        compose.services.remove(0)
    }

    fn paths(service: &Service) -> Vec<&Path> {
        service.env_files.iter().map(|f| f.path.as_path()).collect()
    }

    fn pairs(service: &Service) -> Vec<(&str, Option<&str>)> {
        service
            .environment
            .iter()
            .map(|a| (a.key.as_str(), a.value.as_deref()))
            .collect()
    }

    #[test]
    fn a_mapping_gives_keys_and_values() {
        let service = only_service(
            "services:\n  api:\n    environment:\n      HOST: redis\n      PORT: 6379\n",
        );
        assert_eq!(service.name, "api");
        assert_eq!(
            pairs(&service),
            [("HOST", Some("redis")), ("PORT", Some("6379"))]
        );
    }

    #[test]
    fn a_list_gives_the_same_keys_and_values() {
        let service = only_service(
            "services:\n  api:\n    environment:\n      - HOST=redis\n      - PORT=6379\n",
        );
        assert_eq!(
            pairs(&service),
            [("HOST", Some("redis")), ("PORT", Some("6379"))]
        );
    }

    #[test]
    fn a_bare_name_asks_for_the_host_value() {
        let service = only_service("services:\n  api:\n    environment:\n      - REDIS_HOST\n");
        assert_eq!(pairs(&service), [("REDIS_HOST", None)]);
    }

    #[test]
    fn an_empty_mapping_value_also_asks_for_the_host_value() {
        let service = only_service("services:\n  api:\n    environment:\n      REDIS_HOST:\n");
        assert_eq!(pairs(&service), [("REDIS_HOST", None)]);
    }

    #[test]
    fn an_empty_assignment_is_an_empty_value_not_a_missing_one() {
        let service = only_service("services:\n  api:\n    environment:\n      - TOKEN=\n");
        assert_eq!(pairs(&service), [("TOKEN", Some(""))]);
    }

    #[test]
    fn a_value_keeps_the_equals_signs_inside_it() {
        let service =
            only_service("services:\n  api:\n    environment:\n      - DSN=user=me password=x\n");
        assert_eq!(pairs(&service), [("DSN", Some("user=me password=x"))]);
    }

    #[test]
    fn numbers_and_booleans_arrive_as_the_text_a_container_reads() {
        let service = only_service(
            "services:\n  api:\n    environment:\n      DEBUG: true\n      QUIET: false\n      PORT: 5432\n      RATE: 1.10\n",
        );
        assert_eq!(
            pairs(&service),
            [
                ("DEBUG", Some("true")),
                ("QUIET", Some("false")),
                ("PORT", Some("5432")),
                // Not "1.1": the digits the author wrote are the digits sent.
                ("RATE", Some("1.10"))
            ]
        );
    }

    #[test]
    fn a_word_yaml_might_mistake_for_a_boolean_stays_a_word() {
        // The Norway problem: a parser on YAML 1.1 turns NO into false.
        let service = only_service(
            "services:\n  api:\n    environment:\n      COUNTRY: NO\n      SHIP: yes\n      MODE: off\n",
        );
        assert_eq!(
            pairs(&service),
            [
                ("COUNTRY", Some("NO")),
                ("SHIP", Some("yes")),
                ("MODE", Some("off"))
            ]
        );
    }

    #[test]
    fn a_quoted_number_keeps_every_digit() {
        let service = only_service("services:\n  api:\n    environment:\n      PORT: \"0755\"\n");
        assert_eq!(pairs(&service), [("PORT", Some("0755"))]);
    }

    #[test]
    fn one_env_file_is_a_list_of_one() {
        let service = only_service("services:\n  api:\n    env_file: .env\n");
        assert_eq!(paths(&service), [Path::new(".env")]);
        assert!(service.env_files[0].required);
    }

    #[test]
    fn several_env_files_keep_their_order() {
        let service =
            only_service("services:\n  api:\n    env_file:\n      - .env\n      - .env.local\n");
        assert_eq!(
            paths(&service),
            [Path::new(".env"), Path::new(".env.local")]
        );
    }

    #[test]
    fn the_long_env_file_form_is_read_for_its_path() {
        let service = only_service(
            "services:\n  api:\n    env_file:\n      - path: .env.local\n        required: false\n",
        );
        assert_eq!(paths(&service), [Path::new(".env.local")]);
        // The author said it may be missing, so its absence is a fact, not a doubt.
        assert!(!service.env_files[0].required);
    }

    #[test]
    fn services_keep_the_order_they_were_written_in() {
        let compose = parse_ok(
            "services:\n  web:\n    image: nginx\n  api:\n    image: node\n  db:\n    image: postgres\n",
        );
        let names: Vec<_> = compose.services.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["web", "api", "db"]);
    }

    #[test]
    fn a_service_with_no_environment_still_exists() {
        let service = only_service("services:\n  api:\n    image: node\n");
        assert!(service.environment.is_empty());
        assert!(service.env_files.is_empty());
    }

    #[test]
    fn a_file_with_no_services_is_not_a_failure() {
        assert_eq!(parse_ok("version: '3.8'\n"), Compose::default());
        assert_eq!(parse_ok(""), Compose::default());
    }

    #[test]
    fn an_obsolete_version_key_is_ignored() {
        let compose = parse_ok("version: '3.8'\nservices:\n  api:\n    environment:\n      A: b\n");
        assert_eq!(compose.services.len(), 1);
    }

    #[test]
    fn interpolation_is_left_for_someone_who_knows_the_values() {
        let service =
            only_service("services:\n  api:\n    environment:\n      HOST: ${REDIS_HOST}\n");
        assert_eq!(pairs(&service), [("HOST", Some("${REDIS_HOST}"))]);
    }

    #[test]
    fn yaml_that_does_not_parse_is_reported_as_such() {
        assert!(parse("services:\n  api:\n   - broken\n  \tbad: [").is_err());
    }

    #[test]
    fn a_borrowed_environment_block_arrives_whole() {
        // Sharing a block through an anchor is ordinary in Compose files. Read
        // literally, `<<` is just a key, and every variable under it goes missing.
        let service = only_service(
            "x-common: &common\n  TZ: UTC\n  LANG: C\nservices:\n  api:\n    environment:\n      <<: *common\n      HOST: redis\n",
        );
        assert_eq!(
            pairs(&service),
            [
                ("HOST", Some("redis")),
                ("TZ", Some("UTC")),
                ("LANG", Some("C"))
            ]
        );
    }

    #[test]
    fn what_a_service_states_beats_what_it_borrows() {
        let service = only_service(
            "x-common: &common\n  HOST: localhost\nservices:\n  api:\n    environment:\n      <<: *common\n      HOST: redis\n",
        );
        assert_eq!(pairs(&service), [("HOST", Some("redis"))]);
    }

    #[test]
    fn a_service_may_borrow_its_whole_body() {
        let service = only_service(
            "x-base: &base\n  environment:\n    TZ: UTC\n  env_file: .env\nservices:\n  api:\n    <<: *base\n    image: node\n",
        );
        assert_eq!(pairs(&service), [("TZ", Some("UTC"))]);
        assert_eq!(paths(&service), [Path::new(".env")]);
        assert!(service.env_files[0].required);
    }

    #[test]
    fn several_borrowed_blocks_merge_in_the_order_written() {
        let service = only_service(
            "x-a: &a\n  A: from-a\n  SHARED: from-a\nx-b: &b\n  B: from-b\n  SHARED: from-b\nservices:\n  api:\n    environment:\n      <<: [*a, *b]\n",
        );
        assert_eq!(
            pairs(&service),
            [
                ("A", Some("from-a")),
                ("SHARED", Some("from-a")),
                ("B", Some("from-b"))
            ]
        );
    }
}
