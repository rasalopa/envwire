use std::collections::BTreeSet;
use std::process::Command;

use crate::model::{Origin, Project};
use crate::sources::SourceKind;
use crate::template::Value;

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
    // A project with only an example is unconfigured, not drifting. Saying every
    // promised key is missing would be true and useless.
    if !project.files.iter().any(|f| f.kind == SourceKind::Env) {
        return Vec::new();
    }

    let mut held: BTreeSet<&str> = keys(project, SourceKind::Env).collect();
    // A key a container is really handed is set, wherever it came from. The file
    // supplying it need not be a `.env`, and sending a reader to hunt for a variable
    // their service already receives is the kind of wrong that gets a linter muted.
    held.extend(
        project
            .services
            .iter()
            .flat_map(|service| service.vars.iter())
            .map(|var| var.key.as_str()),
    );
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

/// A key one file sets more than once, where every assignment but the last is dead.
///
/// Not the same as `.env.local` overriding `.env`: overriding across files is what
/// those files are for. Twice in one file is a line somebody edited to no effect.
///
/// Deliberately says nothing about which value won -- see `Value::disclosure`.
pub fn set_twice(project: &Project) -> Vec<Finding> {
    let mut findings = Vec::new();
    for file in &project.files {
        for setting in &file.settings {
            // A bare pass-through is a request, not an assignment: unanswered, Docker
            // drops it and the line above stays alive. Only a real assignment wins.
            if setting.value.is_none() {
                continue;
            }
            let Some(last) = file
                .settings
                .iter()
                .rev()
                .find(|other| other.key == setting.key && other.value.is_some())
            else {
                continue;
            };
            // Only the assignments that lose are worth a word; the last one stands.
            if last.line == setting.line {
                continue;
            }
            findings.push(Finding {
                weight: Weight::Problem,
                what: format!(
                    "{} is set again at line {}, so this line changes nothing",
                    setting.key, last.line
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

/// A file a developer's own values live in, committed to the repository.
///
/// The one finding here that needs no judgement: whatever is in that file is in the
/// history, on every clone, and in every fork. An example file is exempt -- being
/// committed is what it is for.
///
/// Asks git and believes only a clear yes. A repository envwire cannot ask about --
/// no git on the machine, not a work tree, a command that failed for any reason --
/// produces silence. Guessing here would accuse people of leaking secrets they did
/// not leak.
pub fn in_version_control(project: &Project) -> Vec<Finding> {
    let mut findings = Vec::new();
    for file in project.files.iter().filter(|f| f.kind == SourceKind::Env) {
        let (Some(folder), Some(name)) = (file.path.parent(), file.path.file_name()) else {
            continue;
        };
        // The name alone, never the path: git resolves a pathspec against the working
        // directory, and that is already this file's folder. Handing it the whole
        // relative path made git look for `proj/proj/.env`, so a committed file went
        // unreported whenever envwire was pointed at a relative directory.
        let asked = Command::new("git")
            // A folder named `pr[1]` is a path, not a pattern. This one belongs to
            // git itself, before the subcommand, or `ls-files` refuses it outright.
            .arg("--literal-pathspecs")
            .arg("ls-files")
            .arg("--error-unmatch")
            .arg("--")
            .arg(name)
            .current_dir(folder)
            // An inherited GIT_DIR would answer for a repository that is not this one.
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output();
        // Only an answered, successful `yes` counts.
        let tracked = asked.map(|out| out.status.success()).unwrap_or(false);
        if !tracked {
            continue;
        }
        findings.push(Finding {
            weight: Weight::Problem,
            what: format!(
                "{} is committed, so whatever it holds is in the history and every clone",
                file.path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            ),
            at: Origin::File {
                path: file.path.clone(),
            },
            because: None,
        });
    }
    findings
}

/// Key endings that say the value is a secret, whatever came before them.
///
/// The ending is what names a variable's job. `ALLOW_PRIVATE_TARGETS` holds PRIVATE as
/// a whole segment and is a boolean flag -- real, from a project that would have been
/// reported by anything matching anywhere in the name.
const SECRET_ENDING: &[&str] = &[
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "PASSPHRASE",
    "TOKEN",
    "CREDENTIAL",
    "CREDENTIALS",
];

/// Endings of two words, where `KEY` alone would sweep up `SORT_KEY` and `CACHE_KEY`.
const SECRET_KEY_ENDING: &[&str] = &[
    "SECRET",
    "API",
    "PRIVATE",
    "ENCRYPTION",
    "APP",
    "SIGNING",
    "MASTER",
];

/// Values that are a way of saying "nothing is set here".
///
/// Laravel writes `REDIS_PASSWORD=null`; a reader means no password, not a weak one.
/// Calling that a weak secret is wrong twice -- it is not a secret value, and "unset"
/// is a different complaint that deserves its own words.
const NOT_A_SECRET_VALUE: &[&str] = &[
    "null",
    "none",
    "nil",
    "undefined",
    "false",
    "true",
    "yes",
    "no",
    "off",
    "on",
];

/// Values somebody meant to replace and did not.
const LEFT_AS_WRITTEN: &[&str] = &[
    "changeme",
    "change-me",
    "change_me",
    "change-me-in-prod",
    "changethis",
    "secret",
    "password",
    "admin",
    "test",
    "example",
    "placeholder",
    "your-secret-here",
];

/// Below this a secret is guessable by any modern standard.
const SHORT: usize = 16;

/// Whether the value names a file rather than holding a secret.
///
/// `GOOGLE_APPLICATION_CREDENTIALS` ends in CREDENTIALS and holds a path. Measuring
/// how long that path is says nothing about how strong anything is.
fn is_a_path(value: &str) -> bool {
    value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('~')
        || (value.len() > 2 && value.as_bytes()[1] == b':' && value.contains('\\'))
}

/// A secret left at its placeholder, or short enough to guess.
///
/// Never says the value. A finding that quotes the secret it is complaining about puts
/// it in a CI log, which is the thing this tool exists not to do.
///
/// Example files are exempt: a short value there is the point of the file.
pub fn weak_secrets(project: &Project) -> Vec<Finding> {
    let mut findings = Vec::new();

    let mut judge = |key: &str, value: &Value, at: Origin| {
        if !names_a_secret(key) {
            return;
        }
        // A pass-through, a `${VAULT_TOKEN}` and an empty value are rejected together:
        // envwire is not holding a secret to judge in any of the three.
        let Value::Literal(text) = value else { return };
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let plain = text.to_ascii_lowercase();
        if NOT_A_SECRET_VALUE.contains(&plain.as_str())
            || text.chars().all(|c| c.is_ascii_digit())
            || is_a_path(text)
        {
            return;
        }

        if LEFT_AS_WRITTEN.contains(&plain.as_str()) {
            findings.push(Finding {
                weight: Weight::Problem,
                what: format!("{key} is still set to a placeholder"),
                at,
                because: None,
            });
        } else if text.chars().count() < SHORT {
            findings.push(Finding {
                // Short is a judgement, not a breakage. A developer may mean it on
                // their own machine, and it should not fail anybody's build alone.
                weight: Weight::Note,
                what: format!("{key} is shorter than {SHORT} characters"),
                at,
                because: None,
            });
        }
    };

    for file in project.files.iter().filter(|f| f.kind == SourceKind::Env) {
        for setting in &file.settings {
            let Some(value) = &setting.value else {
                continue;
            };
            // Only the assignment that wins is worth judging. `set_twice` already says
            // the earlier one changes nothing, and complaining that a dead line holds a
            // placeholder contradicts it -- nothing runs with that value.
            let wins = file
                .settings
                .iter()
                .rev()
                .find(|other| other.key == setting.key && other.value.is_some())
                .is_some_and(|last| last.line == setting.line);
            if !wins {
                continue;
            }
            judge(
                &setting.key,
                value,
                Origin::Line {
                    path: file.path.clone(),
                    line: setting.line,
                },
            );
        }
    }

    // A secret typed into the Compose file is worse than a short one in a `.env` the
    // repository ignores: this one is committed.
    for service in &project.services {
        for var in &service.vars {
            // An interpolated value is not the value CI will run with: envwire never
            // reads the shell, and `${SECRET:-placeholder}` is exactly how a compose
            // file carries a local default while CI injects the real one.
            if !matches!(var.bound.origin, Origin::Inline { .. })
                || var.bound.via.is_some()
                || var.bound.interpolated
            {
                continue;
            }
            judge(&var.key, &var.bound.value, var.bound.origin.clone());
        }
    }

    findings
}

/// Whether the key's ending says its value is a secret.
fn names_a_secret(key: &str) -> bool {
    let segments: Vec<String> = key.split('_').map(|s| s.to_ascii_uppercase()).collect();
    let Some(last) = segments.last() else {
        return false;
    };
    if SECRET_ENDING.contains(&last.as_str()) {
        return true;
    }
    // `KEY` on its own sweeps up `SORT_KEY` and `PRIMARY_KEY`, so it needs a word in
    // front of it that can only mean a secret.
    last == "KEY"
        && segments
            .len()
            .checked_sub(2)
            .and_then(|i| segments.get(i))
            .is_some_and(|before| SECRET_KEY_ENDING.contains(&before.as_str()))
}

/// Addresses that mean "this very container" once a service is running in one.
///
/// `0.0.0.0` is deliberately absent: inside a container it is the correct address to
/// listen on, and flagging it would tell people to break working services.
const LOOPBACK: &[&str] = &["localhost", "127.0.0.1", "::1"];

/// Key fragments whose value a browser reads, where loopback is the right answer.
///
/// A CORS origin is what the browser sends, and a public endpoint is the address the
/// browser dials -- both are the host's `localhost`, not the container's. Taken from
/// a real project where a rule without this list reported both as bugs.
const BROWSER_FACING: &[&str] = &["PUBLIC", "CORS", "ORIGIN", "BROWSER"];

/// Key endings that can only mean the address this container dials.
///
/// `ENDPOINT` and `URL` are deliberately absent. `MINIO_ENDPOINT` is a dial target and
/// `MINIO_EXTERNAL_ENDPOINT` is a browser's address, and they differ by one word that
/// could have been anything -- envwire cannot tell them apart from a name, so it says
/// nothing about either. A connection string still speaks through its scheme.
const ADDRESS_SLOT: &[&str] = &["HOST", "HOSTNAME", "ADDR", "ADDRESS"];

/// A service told to reach its neighbour at loopback, which is itself.
///
/// Narrow on purpose. Most loopback values a container receives are correct, so the
/// value pointing at loopback is not enough: envwire says something only when the
/// variable also names a service sitting in the same file, which is the evidence that
/// the author meant that service. It reads `service.vars` and never the `.env`, whose
/// loopback values are right for a process run on the host.
pub fn reachable(project: &Project) -> Vec<Finding> {
    let services: Vec<&str> = project
        .services
        .iter()
        .map(|service| service.name.as_str())
        .collect();

    let mut findings = Vec::new();
    for service in &project.services {
        // A container that does not own its network stack reaches something real at
        // loopback: `network_mode: service:redis` shares redis's namespace, and on the
        // host network loopback is the host. Verified by running both.
        if service.network_mode.is_some() {
            continue;
        }
        for var in &service.vars {
            // Something unread could still overrule this, so there is nothing to say.
            if !service.settled(var) {
                continue;
            }
            let Value::Literal(text) = &var.bound.value else {
                continue;
            };
            if browser_facing(&var.key) {
                continue;
            }
            let Some(host) = host_of(text) else { continue };
            if !LOOPBACK.contains(&host) {
                continue;
            }
            let Some(meant) = neighbour(&var.key, text, &service.name, &services) else {
                continue;
            };

            findings.push(Finding {
                weight: Weight::Problem,
                what: format!(
                    "{} in service {} points at {host}, which inside a container is that \
                     container -- and this file has a service named {meant}",
                    var.key, service.name
                ),
                // Point at where the text was typed when that is known.
                at: var
                    .bound
                    .via
                    .clone()
                    .unwrap_or_else(|| var.bound.origin.clone()),
                because: None,
            });
        }
    }
    findings
}

fn browser_facing(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    BROWSER_FACING.iter().any(|word| key.contains(word))
}

/// The service this variable seems to be reaching for, if any names itself.
///
/// Two kinds of evidence, both taken from the text the author wrote: the key names a
/// service (`REDIS_HOST` beside a `redis`), or the URL scheme does
/// (`postgres://...` beside a `postgres`). Without one of them there is nothing but a
/// guess, and a guess here is the expensive kind of wrong.
fn neighbour<'a>(key: &str, value: &str, asker: &str, services: &[&'a str]) -> Option<&'a str> {
    let segments: Vec<&str> = key.split('_').collect();
    let addresses = segments.last().is_some_and(|last| {
        ADDRESS_SLOT
            .iter()
            .any(|slot| last.eq_ignore_ascii_case(slot))
    });

    // Whole segments only. A service named `db` is not named by `SANDBOX_URL`, even
    // though those letters appear in it.
    let named = |candidate: &str| {
        (addresses
            && segments
                .iter()
                .any(|part| part.eq_ignore_ascii_case(candidate)))
            || value
                .split_once("://")
                .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case(candidate))
    };
    // A service naming itself is no evidence of a neighbour: loopback inside the redis
    // container really is redis.
    services
        .iter()
        .copied()
        .filter(|service| *service != asker)
        .find(|service| named(service))
}

/// The host a value points at, when it points at one.
fn host_of(value: &str) -> Option<&str> {
    let after_scheme = match value.split_once("://") {
        Some((_, rest)) => rest,
        None => value,
    };
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    let host = match authority.rsplit_once('@') {
        Some((_, host)) => host,
        None => authority,
    };
    if host.is_empty() {
        return None;
    }
    Some(without_port(host))
}

/// Drop a trailing `:port`, keeping an address that is all colons intact.
fn without_port(host: &str) -> &str {
    if host.starts_with('[') {
        if let Some(close) = host.find(']') {
            return &host[1..close];
        }
    }
    // `::1` is a host, not a host and a port, so only one colon may be a port.
    if host.matches(':').count() == 1 {
        if let Some((before, after)) = host.rsplit_once(':') {
            if !after.is_empty() && after.chars().all(|c| c.is_ascii_digit()) {
                return before;
            }
        }
    }
    host
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

    fn reach(files: &[(&str, &str)]) -> (TempDir, Vec<Finding>) {
        let dir = tempdir().unwrap();
        for (name, body) in files {
            fs::write(dir.path().join(name), body).unwrap();
        }
        let project = model::read(&sources::discover(dir.path())).unwrap();
        let found = reachable(&project);
        (dir, found)
    }

    #[test]
    fn a_service_naming_itself_is_no_evidence_of_a_neighbour() {
        // 127.0.0.1 inside the redis container really is redis. Proven by running it:
        // the healthcheck that dials $REDIS_HOST reports the container healthy.
        let (_dir, found) = reach(&[(
            "docker-compose.yml",
            "services:\n  redis:\n    image: redis\n    environment:\n      REDIS_HOST: 127.0.0.1\n  api:\n    image: node\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_service_sharing_another_namespace_is_left_alone() {
        // `network_mode: service:redis` puts both containers on one network stack, so
        // 127.0.0.1 is exactly how api must reach redis. Verified by running it: both
        // `redis-cli -h 127.0.0.1` and `-h redis` answer PONG.
        let (_dir, found) = reach(&[(
            "docker-compose.yml",
            "services:\n  redis:\n    image: redis\n  api:\n    image: node\n    network_mode: \"service:redis\"\n    environment:\n      REDIS_HOST: 127.0.0.1\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_service_on_the_host_network_is_left_alone() {
        let (_dir, found) = reach(&[(
            "docker-compose.yml",
            "services:\n  redis:\n    image: redis\n  api:\n    image: node\n    network_mode: host\n    environment:\n      REDIS_HOST: localhost\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn only_a_slot_that_plainly_means_an_address_fires_on_the_key() {
        // `MINIO_EXTERNAL_ENDPOINT` and `MINIO_PUBLIC_ENDPOINT` are the same variable
        // with one word changed, and both are read by a browser. envwire cannot tell
        // a dial target from a published address by name, so only the slots that mean
        // nothing else -- HOST, HOSTNAME, ADDR, ADDRESS -- speak.
        for key in ["MINIO_EXTERNAL_ENDPOINT", "FRONTEND_URL", "KEYCLOAK_ISSUER"] {
            let (_dir, found) = reach(&[(
                "docker-compose.yml",
                &format!(
                    "services:\n  minio:\n    image: minio\n  frontend:\n    image: node\n  keycloak:\n    image: kc\n  api:\n    image: node\n    environment:\n      {key}: http://localhost:9000\n"
                ),
            )]);
            assert!(found.is_empty(), "{key} should be silent: {found:?}");
        }
    }

    #[test]
    fn an_address_slot_beside_its_service_still_fires() {
        for key in [
            "REDIS_HOST",
            "REDIS_HOSTNAME",
            "REDIS_ADDR",
            "REDIS_ADDRESS",
        ] {
            let (_dir, found) = reach(&[(
                "docker-compose.yml",
                &format!(
                    "services:\n  redis:\n    image: redis\n  api:\n    image: node\n    environment:\n      {key}: localhost\n"
                ),
            )]);
            assert_eq!(found.len(), 1, "{key} should fire: {found:?}");
        }
    }

    #[test]
    fn a_connection_string_still_fires_whatever_the_key_is_called() {
        // A scheme is unambiguous evidence of dialling, so the key need not be a slot.
        let (_dir, found) = reach(&[(
            "docker-compose.yml",
            "services:\n  postgres:\n    image: postgres\n  api:\n    image: node\n    environment:\n      SOMETHING_ELSE: postgres://u:p@localhost:5432/app\n",
        )]);
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn a_service_dialling_loopback_for_its_neighbour_is_a_problem() {
        let (_dir, found) = reach(&[(
            "docker-compose.yml",
            "services:\n  redis:\n    image: redis\n  api:\n    environment:\n      REDIS_HOST: localhost\n",
        )]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].weight, Weight::Problem);
        assert!(found[0].what.contains("REDIS_HOST"), "{}", found[0].what);
        assert!(found[0].what.contains("redis"), "{}", found[0].what);
    }

    #[test]
    fn a_url_whose_scheme_names_the_neighbour_counts_too() {
        // `DATABASE_URL` names no service, but `postgres://` does.
        let (_dir, found) = reach(&[(
            "docker-compose.yml",
            "services:\n  postgres:\n    image: postgres\n  api:\n    environment:\n      DATABASE_URL: postgres://u:p@localhost:5432/app\n",
        )]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].what.contains("postgres"), "{}", found[0].what);
    }

    #[test]
    fn a_browser_facing_value_is_left_alone() {
        // Taken from a real project: both of these are correct as localhost, and a
        // naive rule flagged both. CORS origins are what the browser sends, and a
        // public endpoint is the address the browser dials.
        let (_dir, found) = reach(&[(
            "docker-compose.yml",
            "services:\n  web:\n    image: node\n  minio:\n    image: minio\n  api:\n    environment:\n      CORS_ORIGINS: http://localhost:3001\n      MINIO_PUBLIC_ENDPOINT: localhost\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn loopback_with_no_neighbour_to_mean_is_left_alone() {
        // Without a service of that name there is no evidence the author meant one.
        let (_dir, found) = reach(&[(
            "docker-compose.yml",
            "services:\n  api:\n    environment:\n      REDIS_HOST: localhost\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_bind_address_is_not_a_destination() {
        // Inside a container 0.0.0.0 is the correct address to listen on.
        let (_dir, found) = reach(&[(
            "docker-compose.yml",
            "services:\n  redis:\n    image: redis\n  api:\n    environment:\n      REDIS_HOST: 0.0.0.0\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn every_loopback_spelling_is_recognised() {
        for spelling in ["localhost", "127.0.0.1", "::1", "[::1]:6379"] {
            let (_dir, found) = reach(&[(
                "docker-compose.yml",
                &format!(
                    "services:\n  redis:\n    image: redis\n  api:\n    environment:\n      REDIS_HOST: \"{spelling}\"\n"
                ),
            )]);
            assert_eq!(found.len(), 1, "{spelling} was not seen: {found:?}");
        }
    }

    #[test]
    fn a_host_that_merely_starts_with_localhost_is_a_different_host() {
        let (_dir, found) = reach(&[(
            "docker-compose.yml",
            "services:\n  redis:\n    image: redis\n  api:\n    environment:\n      REDIS_HOST: localhost.example.com\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_key_that_only_contains_a_service_name_inside_a_word_is_not_a_match() {
        // `SANDBOX_URL` holds the letters of a service named `db`, and matching on
        // substrings rather than whole segments would report it.
        let (_dir, found) = reach(&[(
            "docker-compose.yml",
            "services:\n  db:\n    image: postgres\n  api:\n    environment:\n      SANDBOX_URL: http://localhost:9000\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_value_nothing_could_resolve_is_not_guessed_at() {
        let (_dir, found) = reach(&[(
            "docker-compose.yml",
            "services:\n  redis:\n    image: redis\n  api:\n    environment:\n      - REDIS_HOST\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn something_unread_could_still_overrule_it_so_nothing_is_said() {
        let (_dir, found) = reach(&[(
            "docker-compose.yml",
            "services:\n  redis:\n    image: redis\n  api:\n    env_file:\n      - a.env\n      - gone.env\n",
        )]);
        // Not even the first file exists, so nothing is claimed either way.
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn the_env_file_alone_never_triggers_it() {
        // The `.env` does not reach a container, so localhost there is correct for a
        // process run on the host. This is the false positive that would have fired
        // four times on the first real project.
        let (_dir, found) = reach(&[
            (".env", "REDIS_HOST=localhost\n"),
            (
                "docker-compose.yml",
                "services:\n  redis:\n    image: redis\n  api:\n    image: node\n",
            ),
        ]);
        assert!(found.is_empty(), "{found:?}");
    }

    fn dupes(files: &[(&str, &str)]) -> (TempDir, Vec<Finding>) {
        let dir = tempdir().unwrap();
        for (name, body) in files {
            fs::write(dir.path().join(name), body).unwrap();
        }
        let project = model::read(&sources::discover(dir.path())).unwrap();
        let found = set_twice(&project);
        (dir, found)
    }

    #[test]
    fn a_key_set_twice_in_one_file_is_a_problem() {
        // Real: `ALLOW_PRIVATE_TARGETS` was written twice in one project's `.env`,
        // lines 9 and 24. Whoever edited the first one changed nothing.
        let (_dir, found) = dupes(&[(".env", "A=1\nB=2\nA=3\n")]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].weight, Weight::Problem);
        assert!(found[0].what.contains('A'), "{}", found[0].what);
        // Points at the line that has no effect, not the one that wins.
        assert!(matches!(found[0].at, Origin::Line { line: 1, .. }));
        assert!(found[0].what.contains('3') || found[0].because.is_some());
    }

    #[test]
    fn three_of_the_same_key_report_the_two_that_lose() {
        let (_dir, found) = dupes(&[(".env", "A=1\nA=2\nA=3\n")]);
        assert_eq!(found.len(), 2, "{found:?}");
    }

    #[test]
    fn a_key_set_once_in_each_of_two_files_is_not_a_duplicate() {
        // `.env.local` overriding `.env` is the whole point of that file.
        let (_dir, found) = dupes(&[(".env", "A=1\n"), (".env.local", "A=2\n")]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_example_file_is_held_to_the_same_rule() {
        let (_dir, found) = dupes(&[(".env.example", "A=\nA=\n")]);
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn a_pass_through_beside_an_assignment_is_not_a_duplicate() {
        // Neither order is a key assigned twice: a bare name asks for a value and an
        // assignment gives one. Only two assignments make one of them dead.
        for text in ["TOKEN\nTOKEN=value\n", "TOKEN=value\nTOKEN\n"] {
            let (_dir, found) = dupes(&[(".env", text)]);
            assert!(found.is_empty(), "{text:?}: {found:?}");
        }
    }

    #[test]
    fn no_duplicate_finding_carries_a_value() {
        let (_dir, found) = dupes(&[(".env", "SECRET=first-hunter2\nSECRET=second-hunter2\n")]);
        for finding in &found {
            let said = format!("{} {:?}", finding.what, finding.because);
            assert!(!said.contains("hunter2"), "leaked: {said}");
        }
    }

    #[test]
    fn a_key_a_container_is_handed_counts_as_set() {
        // The env file supplying it is not a `.env`, but the service really receives
        // it, so calling it "not set anywhere" sends a reader hunting for nothing.
        let (_dir, found) = findings(&[
            (".env", "A=1\n"),
            ("svc.env", "SUPPLIED=yes\n"),
            (".env.example", "A=\nSUPPLIED=\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file: svc.env\n",
            ),
        ]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_key_set_inline_in_compose_counts_as_set_too() {
        let (_dir, found) = findings(&[
            (".env", "A=1\n"),
            (".env.example", "A=\nINLINE=\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    environment:\n      INLINE: yes\n",
            ),
        ]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_commented_out_reference_is_not_a_use() {
        // Docker never reads a commented line, so counting it invents a default for a
        // key nothing uses -- turning a real Problem into a Note.
        let (_dir, found) = findings(&[
            (".env", "A=1\n"),
            (".env.example", "A=\nGONE=\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    image: node\n#     GONE: ${GONE:-fallback}\n",
            ),
        ]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].weight, Weight::Problem, "{found:?}");
    }

    #[test]
    fn a_bare_pass_through_does_not_kill_the_assignment_above_it() {
        // A bare name is a request. Unanswered, Docker drops it and the earlier
        // assignment stands, so that earlier line is very much alive.
        let (_dir, found) = dupes(&[(".env", "TOKEN=real\nTOKEN\n")]);
        assert!(found.is_empty(), "{found:?}");
    }

    fn secrets(files: &[(&str, &str)]) -> (TempDir, Vec<Finding>) {
        let dir = tempdir().unwrap();
        for (name, body) in files {
            fs::write(dir.path().join(name), body).unwrap();
        }
        let project = model::read(&sources::discover(dir.path())).unwrap();
        let found = weak_secrets(&project);
        (dir, found)
    }

    #[test]
    fn a_compose_default_is_not_the_value_ci_will_run_with() {
        // `${DB_PASSWORD:-secret}` is how a compose file carries a local default while
        // CI injects the real secret through the shell. envwire never reads the shell,
        // so the default is all it sees -- and failing the build on it breaks exactly
        // the setups that do this right. Verified: with DB_PASSWORD exported, Docker
        // hands the container the exported value, never `secret`.
        let (_dir, found) = secrets(&[(
            "docker-compose.yml",
            "services:\n  api:\n    environment:\n      DB_PASSWORD: ${DB_PASSWORD:-secret}\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_secret_typed_straight_into_compose_is_still_judged() {
        // No interpolation here, so what is written is what the container gets.
        let (_dir, found) = secrets(&[(
            "docker-compose.yml",
            "services:\n  api:\n    environment:\n      DB_PASSWORD: changeme\n",
        )]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].weight, Weight::Problem);
    }

    #[test]
    fn a_dead_assignment_is_not_judged_for_its_value() {
        // `set_twice` already says line 1 changes nothing. Complaining that the line
        // holds a placeholder contradicts it, and nothing runs with that value.
        let (_dir, found) = secrets(&[(
            ".env",
            "DB_PASSWORD=changeme\nDB_PASSWORD=k7Qz2wLm9XvBt4Rp8ScE\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn the_winning_assignment_is_still_judged() {
        let (_dir, found) = secrets(&[(
            ".env",
            "DB_PASSWORD=k7Qz2wLm9XvBt4Rp8ScE\nDB_PASSWORD=changeme\n",
        )]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(matches!(found[0].at, Origin::Line { line: 2, .. }));
    }

    #[test]
    fn a_path_is_not_a_secret_even_when_the_key_sounds_like_one() {
        // `GOOGLE_APPLICATION_CREDENTIALS` ends in CREDENTIALS and holds a file name.
        // Measuring its length says nothing about how strong anything is.
        let (_dir, found) = secrets(&[(
            ".env",
            "GOOGLE_APPLICATION_CREDENTIALS=/etc/gcp.json\nTLS_CERT_PASSWORD=./certs/p.pem\nSSH_KEY_PASSPHRASE=~/.ssh/id\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_secret_left_at_its_placeholder_is_a_problem() {
        let (_dir, found) = secrets(&[(".env", "JWT_SECRET=change-me-in-prod\n")]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].weight, Weight::Problem);
    }

    #[test]
    fn a_short_secret_is_only_a_note() {
        // Short is a judgement, not a breakage: a dev machine may mean it. It should
        // never fail somebody's build on its own.
        let (_dir, found) = secrets(&[(".env", "DB_PASSWORD=hunter22\n")]);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].weight, Weight::Note);
    }

    #[test]
    fn a_long_secret_says_nothing() {
        let (_dir, found) = secrets(&[(
            ".env",
            "JWT_SECRET=3a08ce3caa113c649e12e3eed0d1fcee9d89979a07bfc77ae34e9e38bc40f69a\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_flag_that_merely_contains_a_secret_word_is_not_a_secret() {
        // Real: `ALLOW_PRIVATE_TARGETS=true` holds PRIVATE as a whole segment. Only
        // the ending says what a variable is for.
        let (_dir, found) = secrets(&[(
            ".env",
            "ALLOW_PRIVATE_TARGETS=true\nSORT_KEY=name\nAPI_KEY_ID=abc\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_placeholder_meaning_nothing_is_set_is_not_a_weak_secret() {
        // Real: Laravel writes `REDIS_PASSWORD=null` for "there is no password". That
        // is not a weak secret, and "unset" is a different complaint with other words.
        let (_dir, found) = secrets(&[(
            ".env",
            "REDIS_PASSWORD=null\nMAIL_PASSWORD=none\nDEBUG_TOKEN=false\nRETRY_TOKEN=3\n",
        )]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_secret_nothing_resolves_is_not_guessed_at() {
        let (_dir, found) =
            secrets(&[(".env", "JWT_SECRET=${FROM_VAULT}\nAPI_TOKEN=\nBARE_TOKEN\n")]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn an_example_file_is_where_short_values_belong() {
        let (_dir, found) = secrets(&[
            (".env", "A=1\n"),
            (".env.example", "JWT_SECRET=changeme\nDB_PASSWORD=short\n"),
        ]);
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn a_secret_typed_into_the_compose_file_counts() {
        // Worse than a short one in a gitignored `.env`: this one is committed.
        let (_dir, found) = secrets(&[
            (".env", "A=1\n"),
            (
                "docker-compose.yml",
                "services:\n  minio:\n    environment:\n      MINIO_SECRET_KEY: shortish\n",
            ),
        ]);
        assert_eq!(found.len(), 1, "{found:?}");
    }

    #[test]
    fn no_secret_finding_carries_the_secret() {
        let (_dir, found) = secrets(&[(".env", "JWT_SECRET=hunter2\nAPI_TOKEN=changeme\n")]);
        for finding in &found {
            let said = format!("{} {:?}", finding.what, finding.because);
            assert!(!said.contains("hunter2"), "leaked: {said}");
            assert!(!said.contains("changeme"), "leaked: {said}");
        }
    }

    /// A git repository with `files` in it, some of them committed.
    fn repo(files: &[(&str, &str)], tracked: &[&str]) -> TempDir {
        let dir = tempdir().unwrap();
        for (name, body) in files {
            fs::write(dir.path().join(name), body).unwrap();
        }
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        for name in tracked {
            git(&["add", "--", name]);
        }
        if !tracked.is_empty() {
            git(&["commit", "-qm", "first"]);
        }
        dir
    }

    fn committed(dir: &TempDir) -> Vec<Finding> {
        let project = model::read(&sources::discover(dir.path())).unwrap();
        in_version_control(&project)
    }

    #[test]
    fn a_committed_env_file_is_a_problem() {
        let dir = repo(&[(".env", "SECRET=x\n")], &[".env"]);
        let found = committed(&dir);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].weight, Weight::Problem);
        // The finding is about the file, so it names no line.
        assert!(
            matches!(found[0].at, Origin::File { .. }),
            "{:?}",
            found[0].at
        );
    }

    #[test]
    fn an_ignored_env_file_says_nothing() {
        let dir = repo(
            &[(".env", "SECRET=x\n"), (".gitignore", ".env\n")],
            &[".gitignore"],
        );
        assert!(committed(&dir).is_empty());
    }

    #[test]
    fn a_committed_example_file_is_the_whole_point_of_it() {
        let dir = repo(
            &[
                (".env", "A=1\n"),
                (".env.example", "A=\n"),
                (".gitignore", ".env\n"),
            ],
            &[".env.example", ".gitignore"],
        );
        assert!(committed(&dir).is_empty());
    }

    #[test]
    fn a_project_that_is_not_a_repository_is_not_accused() {
        // No git here at all. Silence is the only honest answer.
        let dir = tempdir().unwrap();
        fs::write(dir.path().join(".env"), "SECRET=x\n").unwrap();
        let project = model::read(&sources::discover(dir.path())).unwrap();
        assert!(in_version_control(&project).is_empty());
    }

    #[test]
    fn no_committed_finding_carries_a_value() {
        let dir = repo(&[(".env", "SECRET=hunter2\n")], &[".env"]);
        for finding in committed(&dir) {
            assert!(
                !finding.what.contains("hunter2"),
                "leaked: {}",
                finding.what
            );
        }
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
        // The project starts without it, so calling it a problem buries the ones that
        // really are. Taken from a real project where 18 of 24 looked like this.
        //
        // The reference sits outside `environment:` on purpose: a key a service is
        // handed is held outright and says nothing at all, so the softened Note is for
        // the keys Compose reads somewhere else -- an image tag, a port, a volume.
        let (_dir, found) = findings(&[
            (".env", "A=1\n"),
            (".env.example", "A=\nTAG=\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    image: app:${TAG:-latest}\n",
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
            (".env.example", "A=\nTAG=\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    image: app:${TAG:-latest}\n    volumes:\n      - ${TAG}:/data\n",
            ),
        ]);
        assert_eq!(found[0].weight, Weight::Problem, "{found:?}");
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
