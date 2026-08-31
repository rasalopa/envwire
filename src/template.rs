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

    fn names_of(text: &str) -> Vec<String> {
        let mut out = Vec::new();
        Template::parse(text).names(&mut out);
        out
    }

    #[test]
    fn text_with_no_dollar_names_nothing() {
        assert!(names_of("postgres://db:5432/app").is_empty());
        assert!(names_of("").is_empty());
    }

    #[test]
    fn both_reference_spellings_are_read() {
        assert_eq!(names_of("${HOST}"), ["HOST"]);
        assert_eq!(names_of("$HOST"), ["HOST"]);
        assert_eq!(names_of("http://${HOST}:6379"), ["HOST"]);
    }

    #[test]
    fn an_operator_is_not_swallowed_into_the_name() {
        // Read as one shape, each of these asks for a variable whose name carries
        // the operator -- "PORT:-5432" -- and is then reported missing from a `.env`
        // that was never meant to carry it.
        assert_eq!(names_of("${PORT:-5432}"), ["PORT"]);
        assert_eq!(names_of("${PORT-5432}"), ["PORT"]);
        assert_eq!(names_of("${FLAG:+on}"), ["FLAG"]);
        assert_eq!(names_of("${FLAG+on}"), ["FLAG"]);
        assert_eq!(names_of("${A:?why}"), ["A"]);
        assert_eq!(names_of("${A?why}"), ["A"]);
    }

    #[test]
    fn a_nested_fallback_is_counted_too() {
        assert_eq!(names_of("${A:-${B}}"), ["A", "B"]);
        assert_eq!(names_of("${A:-${B:-${C}}}"), ["A", "B", "C"]);
    }

    #[test]
    fn a_doubled_dollar_is_never_a_reference() {
        assert!(names_of("pa$$word").is_empty());
        assert!(names_of("$${VAR}").is_empty());
    }

    #[test]
    fn a_shape_we_do_not_recognise_names_nothing() {
        // Inventing a variable out of a shape we cannot read is how a linter starts
        // reporting names nobody wrote.
        for text in [
            "${unclosed",
            "${}",
            "${1BAD}",
            "${A%weird}",
            "100$",
            "$ ",
            "$",
        ] {
            assert!(names_of(text).is_empty(), "{text:?} should name nothing");
        }
    }

    #[test]
    fn every_occurrence_is_counted_once_each() {
        // One entry per occurrence: each sits on a line worth pointing at.
        assert_eq!(names_of("${A}-${B}-${A}"), ["A", "B", "A"]);
    }

    #[test]
    fn a_reference_survives_text_on_both_sides() {
        assert_eq!(names_of("prefix-$A-suffix"), ["A"]);
        assert_eq!(names_of("${A}${B}"), ["A", "B"]);
    }
}
