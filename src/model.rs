use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::compose::{self, KeySource};
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
    Inline { service: String },
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Line { path, line } => write!(f, "{}:{line}", path.display()),
            Origin::Inline { service } => write!(f, "service {service}"),
        }
    }
}

/// A value and where to go to change it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub value: Value,
    /// The place that decided this key.
    pub origin: Origin,
    /// Where the text was typed, when `origin` merely asked for it.
    ///
    /// `HOST: ${DB_HOST}` is decided by the service, but `localhost` was typed at
    /// `.env:3`. Without this a report sends a reader to a Compose line that does not
    /// contain the value it complains about, which is the fastest way to lose them.
    /// Set only when the whole value was one reference: a value glued from text and
    /// several references has no single line worth naming.
    pub via: Option<Origin>,
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
/// Ordered on purpose, and the derived `Ord` is the whole precedence rule:
/// a later `env_file` beats an earlier one, and `environment:` beats every file.
/// Confirmed against `docker compose config`, per key rather than per file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Layer {
    EnvFile(usize),
    Inline,
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
    /// `- ${VAR}=value`. The list form interpolates keys, so envwire does not know
    /// what this service is handed, nor what it overrides.
    DynamicKey(String),
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
    /// Exact rather than conservative: a value that won at `Inline` survives any
    /// unread env file, because nothing an env file says beats `environment:`, and a
    /// value from a later env file survives a gap in an earlier one. A dynamic key
    /// sits at `Inline`, which no layer exceeds, so it quiets the whole service --
    /// which is right, since it could override anything.
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
    /// Every reference in the Compose text and in the env files its services read,
    /// with the line that names it. Duplicates kept.
    ///
    /// Docker interpolates env-file values too, so a `${BASE}` written in one is a
    /// real use of the project `.env`. Scanning only the Compose file would let a
    /// usage check call BASE unused while a container plainly reads it.
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
            let mut references = scan(&raw, path);
            let services = parsed
                .services
                .iter()
                .map(|service| fold(service, path, &interpolation, &mut cache, &mut references))
                .collect();
            (references, services)
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
    interpolation: &Interpolation,
    cache: &mut HashMap<PathBuf, ReadFile>,
    references: &mut Vec<Reference>,
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
    for (index, wanted) in service.env_files.iter().enumerate() {
        let path = anchor.join(&wanted.path);
        env.sources.push(path.clone());

        let layer = Layer::EnvFile(index);
        let read = match cache.get(&path) {
            Some(read) => read.clone(),
            None => match read_env_file(&path) {
                Some(read) => {
                    cache.insert(path.clone(), read.clone());
                    read
                }
                None => {
                    // An optional file the author said might be missing is a resolved
                    // fact, not something envwire failed to read. Verified: Compose
                    // renders such a project without a word.
                    if wanted.required {
                        env.gaps.push(Gap {
                            layer,
                            what: Missing::UnreadFile(path),
                        });
                    }
                    continue;
                }
            },
        };
        references.extend(read.references.iter().cloned());

        for setting in read.settings {
            match setting.value {
                Some(value) => set(
                    &mut env.vars,
                    setting.key,
                    Bound {
                        value,
                        origin: Origin::Line {
                            path: path.clone(),
                            line: setting.line,
                        },
                        via: None,
                    },
                    layer,
                ),
                // A bare name in an env file is a request, not an assignment. Docker
                // answers it from the project `.env` (verified), and drops the key
                // entirely when nothing answers -- which leaves whatever an earlier
                // layer set standing. Overwriting it here erased a real value.
                None => {
                    if let Some(found) = interpolation.get(&setting.key) {
                        set(
                            &mut env.vars,
                            setting.key,
                            Bound {
                                value: found.value.clone(),
                                origin: Origin::Line {
                                    path: path.clone(),
                                    line: setting.line,
                                },
                                via: Some(found.origin.clone()),
                            },
                            layer,
                        );
                    }
                }
            }
        }
    }

    for assignment in &service.environment {
        // Only the list form interpolates keys. A mapping key holding `${...}` is
        // literal text, and calling it dynamic would silence the whole service.
        if assignment.key_source == KeySource::List && assignment.key.contains('$') {
            env.gaps.push(Gap {
                layer: Layer::Inline,
                what: Missing::DynamicKey(assignment.key.clone()),
            });
            continue;
        }

        let bound = match &assignment.value {
            Some(text) => {
                let template = Template::parse(text);
                Bound {
                    value: template
                        .resolve(&|name| interpolation.get(name).map(|bound| bound.value.clone())),
                    origin: Origin::Inline {
                        service: service.name.clone(),
                    },
                    via: template
                        .sole_reference()
                        .and_then(|name| interpolation.get(name))
                        .map(|bound| bound.origin.clone()),
                }
            }
            // A bare `- KEY` is a use, never a definition: the service asks for
            // whatever is around, and what is around is the project `.env`.
            None => {
                let found = interpolation.get(&assignment.key);
                Bound {
                    value: found.map_or(Value::Unknown, |bound| bound.value.clone()),
                    origin: Origin::Inline {
                        service: service.name.clone(),
                    },
                    via: found.map(|bound| bound.origin.clone()),
                }
            }
        };
        set(&mut env.vars, assignment.key.clone(), bound, Layer::Inline);
    }

    env
}

/// One env file, read once and kept for whichever other services name it.
#[derive(Clone)]
struct ReadFile {
    settings: Vec<Setting>,
    references: Vec<Reference>,
}

/// Read an env file a Compose service names, or `None` when it is not there.
///
/// An env file Compose reads is an env file, so it gets the same forgiveness a `.env`
/// does. Its values are scanned as well as parsed: Docker interpolates them against
/// the project `.env`, so a `${BASE}` in one is a real use of BASE.
fn read_env_file(path: &Path) -> Option<ReadFile> {
    let text = std::fs::read_to_string(path).ok()?;
    let (settings, _) = settings_of(dotenv::parse(&text), SourceKind::Env);
    Some(ReadFile {
        references: scan(&text, path),
        settings,
    })
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
                via: None,
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

    fn value(env: &ServiceEnv, key: &str) -> Value {
        var(env, key).bound.value.clone()
    }

    #[test]
    fn a_mapping_key_holding_a_variable_is_literal_text_not_a_gap() {
        // Verified against `docker compose config`: it renders the key back as
        // `$${WHICH}`, which is Compose saying the text is literal. Filing it as a
        // dynamic key would silence every finding for this service.
        let (_dir, project) = project(&[
            (".env", "WHICH=CHOSEN\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    environment:\n      ${WHICH}: a-value\n      PLAIN: v\n",
            ),
        ]);
        let api = service(&project, "api");
        assert!(api.gaps.is_empty(), "{:?}", api.gaps);
        assert!(api.settled(var(api, "PLAIN")));
    }

    #[test]
    fn a_list_key_holding_a_variable_really_is_a_gap() {
        // The list form does interpolate keys, so here envwire genuinely cannot say
        // what the service is handed.
        let (_dir, project) = project(&[(
            "docker-compose.yml",
            "services:\n  api:\n    environment:\n      - ${WHICH}=a-value\n",
        )]);
        let api = service(&project, "api");
        assert_eq!(api.gaps.len(), 1);
        assert!(matches!(api.gaps[0].what, Missing::DynamicKey(_)));
    }

    #[test]
    fn an_env_file_declared_optional_is_not_a_gap_when_absent() {
        // Verified: `docker compose config` renders this cleanly and exits 0. The
        // author said the file may be missing, so its absence is a resolved fact.
        let (_dir, project) = project(&[
            ("base.env", "TOKEN=from-base\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file:\n      - base.env\n      - path: nope.env\n        required: false\n",
            ),
        ]);
        let api = service(&project, "api");
        assert!(api.gaps.is_empty(), "{:?}", api.gaps);
        assert!(api.settled(var(api, "TOKEN")));
    }

    #[test]
    fn a_bare_name_nothing_answers_leaves_the_earlier_value_standing() {
        // Verified: with TOKEN unset everywhere, Docker keeps `from-first`. A bare
        // name is a request, and an unanswered request removes nothing.
        let (_dir, project) = project(&[
            ("one.env", "TOKEN=from-first\n"),
            ("two.env", "TOKEN\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file:\n      - one.env\n      - two.env\n",
            ),
        ]);
        let api = service(&project, "api");
        assert_eq!(
            var(api, "TOKEN").bound.value,
            Value::Literal("from-first".into())
        );
        assert_eq!(var(api, "TOKEN").layer, Layer::EnvFile(0));
    }

    #[test]
    fn a_bare_name_the_project_env_answers_does_win() {
        // Verified: with `.env` holding TOKEN, Docker prefers it over the earlier
        // file, because the request was answered at the later layer.
        let (_dir, project) = project(&[
            (".env", "TOKEN=from-project-dotenv\n"),
            ("one.env", "TOKEN=from-first\n"),
            ("two.env", "TOKEN\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file:\n      - one.env\n      - two.env\n",
            ),
        ]);
        let api = service(&project, "api");
        assert_eq!(
            var(api, "TOKEN").bound.value,
            Value::Literal("from-project-dotenv".into())
        );
        // And the reader is sent to the line that actually holds the text.
        assert!(matches!(
            var(api, "TOKEN").bound.via,
            Some(Origin::Line { line: 1, .. })
        ));
    }

    #[test]
    fn a_variable_named_inside_an_env_file_counts_as_used() {
        // Docker interpolates env-file values against the project `.env`, so BASE is
        // genuinely read here. Missing it makes a usage check call BASE unused.
        let (_dir, project) = project(&[
            (".env", "BASE=resolved\n"),
            ("svc.env", "FROM_FILE=${BASE}/tail\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file: svc.env\n",
            ),
        ]);
        assert!(
            project.references.iter().any(|r| r.name == "BASE"),
            "{:?}",
            project.references
        );
    }

    #[test]
    fn environment_beats_every_env_file() {
        // Verified against `docker compose config`: per key, not per file.
        let (_dir, project) = project(&[
            ("svc.env", "SHARED=from-file\nONLY_FILE=kept\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file: svc.env\n    environment:\n      SHARED: from-inline\n",
            ),
        ]);
        let api = service(&project, "api");
        assert_eq!(value(api, "SHARED"), Value::Literal("from-inline".into()));
        assert_eq!(value(api, "ONLY_FILE"), Value::Literal("kept".into()));
        assert_eq!(var(api, "SHARED").layer, Layer::Inline);
    }

    #[test]
    fn a_later_env_file_beats_an_earlier_one() {
        let (_dir, project) = project(&[
            ("a.env", "K=first\n"),
            ("b.env", "K=second\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file:\n      - a.env\n      - b.env\n",
            ),
        ]);
        let api = service(&project, "api");
        assert_eq!(value(api, "K"), Value::Literal("second".into()));
        assert_eq!(var(api, "K").layer, Layer::EnvFile(1));
    }

    #[test]
    fn a_value_is_resolved_against_the_project_env() {
        // The whole reason `${REDIS_HOST}` must never look like drift: it *is* the
        // .env value, so equality holds by construction.
        let (_dir, project) = project(&[
            (".env", "REDIS_HOST=redis\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    environment:\n      REDIS_HOST: ${REDIS_HOST}\n",
            ),
        ]);
        let api = service(&project, "api");
        assert_eq!(value(api, "REDIS_HOST"), Value::Literal("redis".into()));
    }

    #[test]
    fn a_lone_reference_carries_the_line_its_text_was_typed_on() {
        let (_dir, project) = project(&[
            (".env", "# a note\nDB_HOST=localhost\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    environment:\n      DB_HOST: ${DB_HOST}\n",
            ),
        ]);
        let bound = &var(service(&project, "api"), "DB_HOST").bound;
        // The service decided it, but a reader must be sent where the text is.
        assert!(matches!(bound.origin, Origin::Inline { .. }));
        assert!(matches!(bound.via, Some(Origin::Line { line: 2, .. })));
    }

    #[test]
    fn a_value_glued_from_parts_points_at_no_single_line() {
        let (_dir, project) = project(&[
            (".env", "HOST=db\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    environment:\n      URL: http://${HOST}:5432\n",
            ),
        ]);
        let bound = &var(service(&project, "api"), "URL").bound;
        assert_eq!(bound.value, Value::Literal("http://db:5432".into()));
        assert!(bound.via.is_none());
    }

    #[test]
    fn a_bare_key_asks_the_project_env_and_says_where_it_answered() {
        // Verified against `docker compose config`: a bare name really does pick the
        // .env value up, so the flagship check may fire on it.
        let (_dir, project) = project(&[
            (".env", "DB_HOST=localhost\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    environment:\n      - DB_HOST\n",
            ),
        ]);
        let bound = &var(service(&project, "api"), "DB_HOST").bound;
        assert_eq!(bound.value, Value::Literal("localhost".into()));
        assert!(matches!(bound.via, Some(Origin::Line { line: 1, .. })));
    }

    #[test]
    fn a_bare_key_nothing_answers_is_unknown_not_empty() {
        let (_dir, project) = project(&[(
            "docker-compose.yml",
            "services:\n  api:\n    environment:\n      - NOWHERE\n",
        )]);
        assert_eq!(value(service(&project, "api"), "NOWHERE"), Value::Unknown);
    }

    #[test]
    fn an_env_file_that_is_not_there_is_a_gap_not_an_empty_file() {
        let (_dir, project) = project(&[(
            "docker-compose.yml",
            "services:\n  api:\n    env_file: missing.env\n    environment:\n      K: v\n",
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
    fn a_gap_below_a_value_cannot_overrule_it() {
        let (_dir, project) = project(&[(
            "docker-compose.yml",
            "services:\n  api:\n    env_file: missing.env\n    environment:\n      SAFE: v\n",
        )]);
        let api = service(&project, "api");
        // Nothing an env file says beats `environment:`, so this one is settled.
        assert!(api.settled(var(api, "SAFE")));
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
    fn a_key_named_by_a_variable_is_a_gap_over_the_whole_service() {
        let (_dir, project) = project(&[(
            "docker-compose.yml",
            "services:\n  api:\n    environment:\n      - ${WHICH}=value\n      - PLAIN=v\n",
        )]);
        let api = service(&project, "api");
        assert!(matches!(api.gaps[0].what, Missing::DynamicKey(_)));
        assert_eq!(api.gaps[0].layer, Layer::Inline);
        // It could override anything, so nothing in this service is settled.
        assert!(!api.settled(var(api, "PLAIN")));
    }

    #[test]
    fn keys_keep_the_order_they_were_first_set_in() {
        let (_dir, project) = project(&[
            ("svc.env", "FIRST=1\nSECOND=2\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    env_file: svc.env\n    environment:\n      FIRST: overridden\n      THIRD: 3\n",
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
    fn the_project_env_does_not_leak_into_a_container() {
        // The single largest false-positive generator, closed by construction:
        // verified against `docker compose config` that a .env key nothing names
        // reaches no container at all.
        let (_dir, project) = project(&[
            (".env", "NEVER_ASKED_FOR=x\n"),
            (
                "docker-compose.yml",
                "services:\n  api:\n    environment:\n      K: v\n",
            ),
        ]);
        let api = service(&project, "api");
        assert!(api.vars.iter().all(|v| v.key != "NEVER_ASKED_FOR"));
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
