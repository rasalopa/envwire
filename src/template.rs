/// What a variable will hold, as far as envwire can honestly say.
///
/// The third state is the absence of a key from a map. Keeping it out of the enum is
/// what stops a check reading an unresolved reference as `Literal("")`: an empty
/// value is a variable the container has, an absent one is a variable it does not,
/// and `Unknown` is envwire admitting it cannot see the shell that will run
/// `docker compose up`. Collapse any two of the three and the checks start
/// describing containers nobody is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Literal(String),
    Unknown,
}

impl Value {
    /// What a line of a `.env`-shaped file amounts to.
    ///
    /// A value naming any variable at all is `Unknown` and stays that way. Compose
    /// does expand `${OTHER}` inside a `.env` value, but only when the value was
    /// unquoted or double-quoted, and dotenv.rs strips the quotes before this sees
    /// the text -- so a single-quoted `'${HOST}/x'`, which Docker keeps literal,
    /// arrives here indistinguishable from a double-quoted one it would expand.
    /// Guessing buys one resolved value and risks every finding built on it.
    pub fn stated(text: &str) -> Value {
        let parsed = Template::parse(text);
        if parsed.has_refs() {
            Value::Unknown
        } else {
            // Reference-free, so this only collapses `$$` into `$`.
            parsed.resolve(&|_| None)
        }
    }

    /// All envwire will say about a value out loud.
    ///
    /// Never the value. This tool reads the file the secrets live in, and
    /// `envwire check` is built to run in CI, where whatever it prints lands in a
    /// build log that outlives the run and, on a public repository, is readable by
    /// anyone. A linter that leaks the credentials it was pointed at is worse than
    /// no linter. Whether a value is there is the whole of what a reader needs; a
    /// check that must name a value -- a host, a URL -- says it itself, and answers
    /// for that choice on its own.
    pub fn disclosure(&self) -> &'static str {
        match self {
            Value::Literal(text) if text.is_empty() => "set, empty",
            Value::Literal(_) => "set",
            Value::Unknown => "not set here",
        }
    }
}

/// A value out of a Compose file, split into text and the variables it names.
///
/// Kept as a parse rather than a string because the string lies twice:
/// `${REDIS_HOST}` compared against `.env` looks like drift when it *is* the `.env`
/// value, and `pa$$word` compared against `pa$word` looks like drift when they are
/// the same password.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Template(Vec<Segment>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Text(String),
    Reference { name: String, fallback: Fallback },
}

/// Private. No check walks a template; they read a [`Value`].
///
/// All eight forms are told apart even where envwire resolves none of them: read as
/// one shape, `${PORT:-5432}` becomes a reference to a variable literally named
/// `PORT:-5432`, which is then reported missing from a `.env` that was never meant
/// to carry it. The fallback holds a parsed `Template` rather than text, so
/// `${A:-${B}}` still records a use of B.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Fallback {
    /// `$VAR`, `${VAR}`, `${VAR?e}`, `${VAR:?e}`. The `?` forms abort Compose
    /// instead of substituting, which from here is indistinguishable from having
    /// nothing to fall back on.
    None,
    /// `${VAR-x}`, and `${VAR:-x}` when `on_empty`.
    Default {
        on_empty: bool,
        value: Box<Template>,
    },
    /// `${VAR+x}`, and `${VAR:+x}` when `on_empty`.
    Alternate {
        on_empty: bool,
        value: Box<Template>,
    },
}

impl Template {
    /// Split `text` at every `$` Compose would act on.
    ///
    /// `$$` is consumed whole and yields one literal `$`, so `$${VAR}` is the text
    /// `${VAR}` and never a reference -- miss that and envwire reports a phantom use
    /// of VAR and phantom drift against a `.env` spelling the same password without
    /// the escape. An unclosed `${`, a name no shell would accept, and an operator
    /// Compose does not have all stay literal text: inventing a variable out of a
    /// shape we do not recognise is how a linter starts reporting names nobody wrote.
    pub fn parse(text: &str) -> Template {
        let mut out = Template::default();
        let mut literal = String::new();
        let mut rest = text;

        while let Some(at) = rest.find('$') {
            literal.push_str(&rest[..at]);
            let after = &rest[at + 1..];

            if let Some(tail) = after.strip_prefix('$') {
                literal.push('$');
                rest = tail;
                continue;
            }

            if let Some(body) = after.strip_prefix('{') {
                if let Some(end) = closing_brace(body) {
                    if let Some(segment) = reference(&body[..end]) {
                        out.push_text(&mut literal);
                        out.0.push(segment);
                        rest = &body[end + 1..];
                        continue;
                    }
                }
                literal.push_str("${");
                rest = body;
                continue;
            }

            let len = leading_name(after);
            if len > 0 {
                out.push_text(&mut literal);
                out.0.push(Segment::Reference {
                    name: after[..len].to_string(),
                    fallback: Fallback::None,
                });
                rest = &after[len..];
                continue;
            }

            literal.push('$');
            rest = after;
        }

        literal.push_str(rest);
        out.push_text(&mut literal);
        out
    }

    fn push_text(&mut self, literal: &mut String) {
        if !literal.is_empty() {
            self.0.push(Segment::Text(std::mem::take(literal)));
        }
    }

    fn has_refs(&self) -> bool {
        self.0
            .iter()
            .any(|s| matches!(s, Segment::Reference { .. }))
    }

    /// What this becomes, given a way to look a variable up.
    ///
    /// One `Unknown` poisons the whole value. Half a value is not a value:
    /// `http://${HOST}:5432` with an unknown host must never reach a comparison as
    /// `http://:5432`.
    pub fn resolve(&self, lookup: &dyn Fn(&str) -> Option<Value>) -> Value {
        let mut out = String::new();
        for segment in &self.0 {
            let piece = match segment {
                Segment::Text(text) => Value::Literal(text.clone()),
                Segment::Reference { name, fallback } => match (fallback, lookup(name)) {
                    (_, Some(Value::Unknown)) => Value::Unknown,

                    (Fallback::Default { on_empty, value }, Some(Value::Literal(found))) => {
                        if found.is_empty() && *on_empty {
                            value.resolve(lookup)
                        } else {
                            Value::Literal(found)
                        }
                    }
                    (Fallback::Default { value, .. }, None) => value.resolve(lookup),

                    (Fallback::Alternate { on_empty, value }, Some(Value::Literal(found))) => {
                        if found.is_empty() && *on_empty {
                            Value::Literal(String::new())
                        } else {
                            value.resolve(lookup)
                        }
                    }
                    // Where a default gives envwire something to say, an alternate
                    // gives it only emptiness, and "this variable is empty" is not
                    // worth being wrong about.
                    (Fallback::Alternate { .. }, None) => Value::Unknown,

                    (Fallback::None, Some(found)) => found,
                    (Fallback::None, None) => Value::Unknown,
                },
            };
            match piece {
                Value::Literal(text) => out.push_str(&text),
                Value::Unknown => return Value::Unknown,
            }
        }
        Value::Literal(out)
    }

    /// Every variable this text names, nested fallbacks included.
    ///
    /// Duplicates are kept: the caller wants one entry per occurrence, because each
    /// occurrence sits on a line worth pointing at.
    pub fn names(&self, out: &mut Vec<String>) {
        for segment in &self.0 {
            if let Segment::Reference { name, fallback } = segment {
                out.push(name.clone());
                match fallback {
                    Fallback::Default { value, .. } | Fallback::Alternate { value, .. } => {
                        value.names(out);
                    }
                    Fallback::None => {}
                }
            }
        }
    }
}

/// Where the brace opened at the start of `body` closes, counting the nested ones.
fn closing_brace(body: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in body.char_indices() {
        match c {
            '{' => depth += 1,
            '}' if depth == 0 => return Some(i),
            '}' => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Read what sits between `${` and its `}`.
fn reference(inner: &str) -> Option<Segment> {
    let len = leading_name(inner);
    if len == 0 {
        return None;
    }
    let (name, rest) = inner.split_at(len);

    // Two-character operators first, or `:-` is read as a bare `:`.
    let fallback = if rest.is_empty() {
        Fallback::None
    } else if let Some(value) = rest.strip_prefix(":-") {
        Fallback::Default {
            on_empty: true,
            value: Box::new(Template::parse(value)),
        }
    } else if let Some(value) = rest.strip_prefix(":+") {
        Fallback::Alternate {
            on_empty: true,
            value: Box::new(Template::parse(value)),
        }
    } else if let Some(value) = rest.strip_prefix('-') {
        Fallback::Default {
            on_empty: false,
            value: Box::new(Template::parse(value)),
        }
    } else if let Some(value) = rest.strip_prefix('+') {
        Fallback::Alternate {
            on_empty: false,
            value: Box::new(Template::parse(value)),
        }
    } else if rest.starts_with(":?") || rest.starts_with('?') {
        Fallback::None
    } else {
        // An operator Compose does not have. Leave the whole thing as text.
        return None;
    };

    Some(Segment::Reference {
        name: name.to_string(),
        fallback,
    })
}

/// How much of `text` is a name a shell would accept.
fn leading_name(text: &str) -> usize {
    let mut len = 0;
    for (i, c) in text.char_indices() {
        let ok = if i == 0 {
            c.is_ascii_alphabetic() || c == '_'
        } else {
            c.is_ascii_alphanumeric() || c == '_'
        };
        if !ok {
            break;
        }
        len = i + c.len_utf8();
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve against a small table, the way the project `.env` will.
    fn with(pairs: &[(&str, &str)], text: &str) -> Value {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        Template::parse(text).resolve(&|name| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| Value::Literal(v.clone()))
        })
    }

    fn literal(text: &str) -> Value {
        Value::Literal(text.to_string())
    }

    fn names_of(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        Template::parse(text).names(&mut out);
        out
    }

    #[test]
    fn text_with_no_dollar_is_carried_through() {
        assert_eq!(
            with(&[], "postgres://db:5432/app"),
            literal("postgres://db:5432/app")
        );
        assert!(names_of("plain text").is_empty());
    }

    #[test]
    fn a_reference_becomes_the_value_it_names() {
        assert_eq!(with(&[("HOST", "redis")], "${HOST}"), literal("redis"));
        assert_eq!(with(&[("HOST", "redis")], "$HOST"), literal("redis"));
    }

    #[test]
    fn a_reference_glued_to_text_keeps_both() {
        assert_eq!(
            with(&[("HOST", "redis")], "http://${HOST}:6379"),
            literal("http://redis:6379")
        );
    }

    #[test]
    fn an_unknown_reference_poisons_the_whole_value() {
        // Never "http://:6379": half a value would be compared as if it were one.
        assert_eq!(with(&[], "http://${HOST}:6379"), Value::Unknown);
    }

    #[test]
    fn a_default_is_used_only_when_there_is_nothing_to_use() {
        assert_eq!(with(&[], "${PORT:-5432}"), literal("5432"));
        assert_eq!(with(&[("PORT", "6000")], "${PORT:-5432}"), literal("6000"));
    }

    #[test]
    fn the_two_default_forms_disagree_about_empty() {
        assert_eq!(with(&[("P", "")], "${P:-fallback}"), literal("fallback"));
        assert_eq!(with(&[("P", "")], "${P-fallback}"), literal(""));
    }

    #[test]
    fn an_alternate_replaces_a_value_that_is_there() {
        assert_eq!(with(&[("FLAG", "1")], "${FLAG:+on}"), literal("on"));
        assert_eq!(with(&[("FLAG", "")], "${FLAG:+on}"), literal(""));
        assert_eq!(with(&[("FLAG", "")], "${FLAG+on}"), literal("on"));
        // Absent: an alternate offers only emptiness, not worth being wrong about.
        assert_eq!(with(&[], "${FLAG:+on}"), Value::Unknown);
    }

    #[test]
    fn the_error_forms_have_nothing_to_fall_back_on() {
        assert_eq!(with(&[("A", "x")], "${A:?required}"), literal("x"));
        assert_eq!(with(&[], "${A:?required}"), Value::Unknown);
        assert_eq!(with(&[], "${A?required}"), Value::Unknown);
    }

    #[test]
    fn an_operator_is_not_swallowed_into_the_name() {
        // Read as one shape this asks for a variable named "PORT:-5432".
        assert_eq!(names_of("${PORT:-5432}"), ["PORT"]);
        assert_eq!(names_of("${A-x}"), ["A"]);
        assert_eq!(names_of("${A:?why}"), ["A"]);
    }

    #[test]
    fn a_nested_default_is_resolved_and_counted() {
        assert_eq!(with(&[("B", "second")], "${A:-${B}}"), literal("second"));
        assert_eq!(with(&[], "${A:-${B:-third}}"), literal("third"));
        assert_eq!(names_of("${A:-${B}}"), ["A", "B"]);
    }

    #[test]
    fn a_doubled_dollar_is_one_dollar_and_never_a_reference() {
        assert_eq!(with(&[("VAR", "x")], "pa$$word"), literal("pa$word"));
        assert_eq!(with(&[("VAR", "x")], "$${VAR}"), literal("${VAR}"));
        assert!(names_of("$${VAR}").is_empty());
    }

    #[test]
    fn a_shape_we_do_not_recognise_stays_text() {
        for text in ["${unclosed", "${}", "${1BAD}", "${A%weird}", "100$"] {
            assert_eq!(
                with(&[], text),
                literal(text),
                "{text:?} should stay literal"
            );
            assert!(names_of(text).is_empty(), "{text:?} names nothing");
        }
    }

    #[test]
    fn every_occurrence_is_counted_once_each() {
        assert_eq!(names_of("${A}-${B}-${A}"), ["A", "B", "A"]);
    }

    #[test]
    fn a_value_that_names_nothing_is_stated_as_written() {
        assert_eq!(
            Value::stated("postgres://db/app"),
            literal("postgres://db/app")
        );
        assert_eq!(Value::stated(""), literal(""));
        assert_eq!(Value::stated("pa$$word"), literal("pa$word"));
    }

    #[test]
    fn a_value_that_names_a_variable_is_not_stated_at_all() {
        // dotenv.rs has already dropped the quotes, so there is no way to tell a
        // literal single-quoted value from a double-quoted one Docker expands.
        assert_eq!(Value::stated("${HOST}/x"), Value::Unknown);
        assert_eq!(Value::stated("$HOST"), Value::Unknown);
    }

    #[test]
    fn nothing_a_value_holds_is_ever_disclosed() {
        // The rule this pins: envwire reads the file the secrets live in, and its
        // CI mode prints into build logs. If this test starts failing because a
        // value leaked into the wording, that is the bug, not the test.
        let secrets = [
            "hunter2",
            "sk-live-9f3a2b",
            "postgres://user:pa55@db/app",
            "-----BEGIN PRIVATE KEY-----",
        ];
        for secret in secrets {
            let said = Value::Literal(secret.to_string()).disclosure();
            assert!(!said.contains(secret), "{said:?} disclosed {secret:?}");
            assert_eq!(said, "set");
        }
        assert_eq!(Value::Literal(String::new()).disclosure(), "set, empty");
        assert_eq!(Value::Unknown.disclosure(), "not set here");
    }

    #[test]
    fn a_lookup_that_answers_unknown_is_believed() {
        let unknown = Template::parse("${A}").resolve(&|_| Some(Value::Unknown));
        assert_eq!(unknown, Value::Unknown);
        // Even behind a default: the variable is set, we just cannot read it.
        let behind = Template::parse("${A:-x}").resolve(&|_| Some(Value::Unknown));
        assert_eq!(behind, Value::Unknown);
    }
}
