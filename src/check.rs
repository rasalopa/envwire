use std::collections::BTreeSet;

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
            let Some(last) = file
                .settings
                .iter()
                .rev()
                .find(|other| other.key == setting.key)
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
    fn a_pass_through_beside_an_assignment_still_counts() {
        let (_dir, found) = dupes(&[(".env", "TOKEN\nTOKEN=value\n")]);
        assert_eq!(found.len(), 1, "{found:?}");
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
