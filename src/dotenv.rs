use std::fs;
use std::path::Path;

use crate::error::{Error, Result};

/// One `KEY=value` a file states.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: String,
    /// 1-based, so a finding can send a reader to the line they will open.
    pub line: usize,
}

/// A line that meant to be a setting and did not parse as one.
///
/// Kept rather than dropped: a typo in a `.env` is the kind of thing the tool is
/// there to notice, and silence would let it read as a missing variable instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed {
    pub line: usize,
    pub text: String,
}

/// Everything one `.env`-shaped file says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Document {
    /// In file order, duplicates kept: which of two assignments wins is a
    /// finding, and collapsing them here would throw the evidence away.
    pub entries: Vec<Entry>,
    pub malformed: Vec<Malformed>,
}

/// Read and parse a `.env`-shaped file.
pub fn read(path: &Path) -> Result<Document> {
    let text = fs::read_to_string(path).map_err(|source| Error::Read {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(parse(&text))
}

/// Parse `.env` text.
///
/// Deliberately forgiving: envwire reports on projects it did not write, and a
/// parser that gives up on the first odd line would report a whole file as missing.
pub fn parse(text: &str) -> Document {
    let mut doc = Document::default();
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0;

    while index < lines.len() {
        let line_no = index + 1;
        let raw = lines[index];
        index += 1;

        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // `export KEY=value` is how the same file gets sourced by a shell.
        let body = match trimmed.strip_prefix("export ") {
            Some(rest) => rest.trim_start(),
            None => trimmed,
        };

        let Some((raw_key, raw_value)) = body.split_once('=') else {
            doc.malformed.push(Malformed {
                line: line_no,
                text: trimmed.to_string(),
            });
            continue;
        };

        let key = raw_key.trim();
        if !is_name(key) {
            doc.malformed.push(Malformed {
                line: line_no,
                text: trimmed.to_string(),
            });
            continue;
        }

        // A value may open a quote and close it several lines down: certificates
        // and private keys live in .env files exactly that way.
        let (value, consumed) = read_value(raw_value, &lines[index..]);
        index += consumed;

        doc.entries.push(Entry {
            key: key.to_string(),
            value,
            line: line_no,
        });
    }

    doc
}

/// Whether `name` can be an environment variable name.
pub(crate) fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Resolve the value that starts at `first`, borrowing `rest` when a quote stays open.
///
/// Returns the value and how many extra lines it swallowed.
fn read_value(first: &str, rest: &[&str]) -> (String, usize) {
    let start = first.trim_start();

    let Some(quote) = start.chars().next().filter(|c| *c == '"' || *c == '\'') else {
        return (unquoted(first), 0);
    };

    let after_quote = &start[quote.len_utf8()..];
    if let Some(end) = closing_quote(after_quote, quote) {
        return (finish(&after_quote[..end], quote), 0);
    }

    // The quote is still open. Take whole lines until one closes it; if none does,
    // the file is truncated or the quote was never meant to open, so keep the
    // single line rather than swallowing the rest of the file.
    let mut collected = String::from(after_quote);
    for (taken, line) in rest.iter().enumerate() {
        collected.push('\n');
        if let Some(end) = closing_quote(line, quote) {
            collected.push_str(&line[..end]);
            return (finish(&collected, quote), taken + 1);
        }
        collected.push_str(line);
    }

    (finish(after_quote, quote), 0)
}

/// Where `quote` closes in `text`, skipping the ones a backslash escapes.
fn closing_quote(text: &str, quote: char) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // Single quotes carry no escapes, so a backslash there is just a byte.
            b'\\' if quote == '"' => i += 2,
            c if c == quote as u8 => return Some(i),
            _ => i += 1,
        }
    }
    None
}

/// Apply the escapes a double-quoted value carries; single quotes stay literal.
fn finish(body: &str, quote: char) -> String {
    if quote == '\'' {
        return body.to_string();
    }

    let mut out = String::with_capacity(body.len());
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            // An escape envwire does not know is left as the author wrote it.
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Trim a bare value and drop the comment a reader would see trailing it.
///
/// Only whitespace then `#` starts a comment: `PASSWORD=hunter2#4` is a password,
/// not a password and an opinion.
fn unquoted(value: &str) -> String {
    let value = value.trim();
    let bytes = value.as_bytes();
    for (i, &c) in bytes.iter().enumerate() {
        if c == b'#' && i > 0 && bytes[i - 1].is_ascii_whitespace() {
            return value[..i].trim_end().to_string();
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(text: &str) -> Vec<(String, String)> {
        parse(text)
            .entries
            .into_iter()
            .map(|e| (e.key, e.value))
            .collect()
    }

    fn one(text: &str) -> String {
        let doc = parse(text);
        assert_eq!(doc.entries.len(), 1, "expected one entry in {text:?}");
        doc.entries[0].value.clone()
    }

    #[test]
    fn a_plain_assignment_is_a_key_and_a_value() {
        assert_eq!(
            values("DATABASE_URL=postgres://localhost/app"),
            [("DATABASE_URL".into(), "postgres://localhost/app".into())]
        );
    }

    #[test]
    fn blank_lines_and_comments_say_nothing() {
        let doc = parse("# a note\n\n   \nKEY=value\n# trailing note\n");
        assert_eq!(doc.entries.len(), 1);
        assert!(doc.malformed.is_empty());
    }

    #[test]
    fn space_around_the_equals_is_not_part_of_either_side() {
        assert_eq!(
            values("  KEY  =  value  "),
            [("KEY".into(), "value".into())]
        );
    }

    #[test]
    fn an_exported_line_is_still_an_assignment() {
        assert_eq!(values("export KEY=value"), [("KEY".into(), "value".into())]);
    }

    #[test]
    fn an_empty_value_is_a_value() {
        assert_eq!(values("KEY="), [("KEY".into(), String::new())]);
    }

    #[test]
    fn quotes_wrap_a_value_without_becoming_part_of_it() {
        assert_eq!(one(r#"KEY="value""#), "value");
        assert_eq!(one("KEY='value'"), "value");
    }

    #[test]
    fn a_quoted_value_keeps_the_space_it_was_given() {
        assert_eq!(one(r#"KEY="  padded  ""#), "  padded  ");
    }

    #[test]
    fn double_quotes_spend_their_escapes() {
        assert_eq!(one(r#"KEY="line\nbreak""#), "line\nbreak");
        assert_eq!(one(r#"KEY="a\tb""#), "a\tb");
        assert_eq!(one(r#"KEY="say \"hi\"""#), r#"say "hi""#);
    }

    #[test]
    fn single_quotes_keep_everything_literal() {
        assert_eq!(one(r"KEY='line\nbreak'"), r"line\nbreak");
    }

    #[test]
    fn an_unknown_escape_survives_as_written() {
        assert_eq!(one(r#"KEY="C:\path""#), r"C:\path");
    }

    #[test]
    fn a_trailing_comment_is_not_part_of_the_value() {
        assert_eq!(one("KEY=value # why it is set"), "value");
    }

    #[test]
    fn a_hash_inside_a_value_stays_in_the_value() {
        assert_eq!(one("PASSWORD=hunter2#4"), "hunter2#4");
        assert_eq!(one(r#"PASSWORD="hunter2 # 4""#), "hunter2 # 4");
    }

    #[test]
    fn a_value_may_run_past_the_line_it_started_on() {
        let doc = parse("KEY=\"-----BEGIN-----\nmiddle\n-----END-----\"\nNEXT=after\n");
        assert_eq!(doc.entries.len(), 2);
        assert_eq!(
            doc.entries[0].value,
            "-----BEGIN-----\nmiddle\n-----END-----"
        );
        assert_eq!(doc.entries[1].key, "NEXT");
        assert_eq!(doc.entries[1].line, 4);
        assert!(doc.malformed.is_empty());
    }

    #[test]
    fn a_quote_that_never_closes_does_not_eat_the_file() {
        let doc = parse("KEY=\"unterminated\nNEXT=after\n");
        assert_eq!(doc.entries.len(), 2);
        assert_eq!(doc.entries[1].key, "NEXT");
    }

    #[test]
    fn a_line_without_an_equals_is_reported_not_dropped() {
        let doc = parse("JUST_A_WORD\nKEY=value\n");
        assert_eq!(doc.entries.len(), 1);
        assert_eq!(
            doc.malformed,
            [Malformed {
                line: 1,
                text: "JUST_A_WORD".into()
            }]
        );
    }

    #[test]
    fn a_name_no_shell_would_accept_is_malformed() {
        for bad in ["1KEY=value", "MY KEY=value", "=value", "KEY-NAME=value"] {
            let doc = parse(bad);
            assert!(doc.entries.is_empty(), "{bad:?} should not parse");
            assert_eq!(doc.malformed.len(), 1, "{bad:?} should be reported");
        }
    }

    #[test]
    fn every_entry_remembers_the_line_it_came_from() {
        let doc = parse("# note\nFIRST=1\n\nSECOND=2\n");
        assert_eq!(doc.entries[0].line, 2);
        assert_eq!(doc.entries[1].line, 4);
    }

    #[test]
    fn both_halves_of_a_repeated_key_are_kept() {
        // Which assignment wins is a finding of its own; collapsing them here
        // would throw away the evidence that there were two.
        assert_eq!(
            values("KEY=first\nKEY=second\n"),
            [
                ("KEY".into(), "first".into()),
                ("KEY".into(), "second".into())
            ]
        );
    }

    #[test]
    fn carriage_returns_do_not_end_up_in_values() {
        assert_eq!(values("KEY=value\r\nOTHER=2\r\n")[0].1, "value");
    }

    #[test]
    fn an_empty_file_says_nothing() {
        assert_eq!(parse(""), Document::default());
    }
}
