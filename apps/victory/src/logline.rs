//! Reading the lines a run logs: the tracing prefix, colour codes and
//! `key=value` fields, shared by `victory ab` and `victory report`.
//!
//! A line as `victory_core::logging` writes it (the fmt layer's default
//! format) is `<RFC 3339 time> <level, right-aligned to 5> <target>: <message>
//! <key=value ...>`, e.g. `2026-11-26T04:32:15.284647Z  INFO
//! victory_gpu::framelog: frame times: ...`. String fields shown with `?`
//! are quoted with Rust's escapes. Lines in another shape (subcommand output,
//! the title's own output) are not log lines.

use std::borrow::Cow;

/// `line` without ANSI colour sequences (`ESC [ ... final byte`) and without
/// a leading byte order mark. A log redirected to a file has no colour since
/// the logger checks for a console; a log captured otherwise may.
pub fn clean(line: &str) -> Cow<'_, str> {
    let line = line.strip_prefix('\u{feff}').unwrap_or(line);
    if !line.contains('\u{1b}') {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            // Parameters and intermediates, then one final byte in @..~.
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
    }
    Cow::Owned(out)
}

/// One log line taken apart.
#[derive(Debug, PartialEq)]
pub struct Line<'a> {
    pub time: &'a str,
    pub level: &'a str,
    pub target: &'a str,
    /// The message and the fields after it.
    pub rest: &'a str,
}

const LEVELS: [&str; 5] = ["ERROR", "WARN", "INFO", "DEBUG", "TRACE"];

/// `line` (already [`clean`]ed) as a log line, or `None` for any other line.
pub fn parse(line: &str) -> Option<Line<'_>> {
    let (time, after) = line.split_once(' ')?;
    // 2026-11-26T04:32:15.284647Z: a date, a `T`, a time.
    if time.len() < 20
        || time.as_bytes().get(10) != Some(&b'T')
        || !time.as_bytes()[0].is_ascii_digit()
    {
        return None;
    }
    let after = after.trim_start();
    let (level, after) = after.split_once(' ')?;
    if !LEVELS.contains(&level) {
        return None;
    }
    let after = after.trim_start();
    let (target, rest) = after.split_once(": ")?;
    if target.is_empty() || target.contains(' ') {
        return None;
    }
    Some(Line {
        time,
        level,
        target,
        rest,
    })
}

/// The value of field `key` in `rest`, unquoted and unescaped, or `None`.
/// A field begins after a space (fields follow the message) and a quoted
/// value runs to its closing quote, so values with spaces are whole.
pub fn field(rest: &str, key: &str) -> Option<String> {
    let needle = format!(" {key}=");
    let at = rest.find(&needle)? + needle.len();
    let value = &rest[at..];
    if let Some(quoted) = value.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = quoted.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => return Some(out),
                '\\' => match chars.next()? {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    other => out.push(other),
                },
                c => out.push(c),
            }
        }
        // No closing quote: the line was cut.
        return Some(out);
    }
    let end = value.find(' ').unwrap_or(value.len());
    Some(value[..end].to_owned())
}

/// The numbers after `"; each: "` in a message that starts with `head`
/// (the frame log's `frame times:` and `frame draws:` lines).
pub fn each(rest: &str, head: &str) -> Option<Vec<f64>> {
    if !rest.starts_with(head) {
        return None;
    }
    let at = rest.find("; each:")? + "; each:".len();
    rest[at..]
        .split_whitespace()
        .map(|v| v.parse::<f64>().ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "2026-11-26T04:32:10.590339Z  INFO victory_gpu::pipeline: first bind pipeline=0x222cf05a070 frame=2 stage=\"vertex\" size=992 hash=dfafaf385b2f94e8";

    #[test]
    fn a_real_line_is_taken_apart() {
        let line = parse(REAL).unwrap();
        assert_eq!(line.time, "2026-11-26T04:32:10.590339Z");
        assert_eq!(line.level, "INFO");
        assert_eq!(line.target, "victory_gpu::pipeline");
        assert!(line.rest.starts_with("first bind pipeline="));
        assert_eq!(field(line.rest, "stage").as_deref(), Some("vertex"));
        assert_eq!(field(line.rest, "frame").as_deref(), Some("2"));
        assert_eq!(
            field(line.rest, "hash").as_deref(),
            Some("dfafaf385b2f94e8")
        );
        assert_eq!(
            parse("mapped at      0x7ffdaf6c0000 (preferred 0x140000000)"),
            None
        );
        assert_eq!(
            parse("title dll      bink2scarlett.dll at 0x7ffeb5d80000"),
            None
        );
    }

    #[test]
    fn colour_and_a_byte_order_mark_are_taken_off() {
        // As the fmt layer colours: dimmed time, green level, italic field names.
        let coloured = "\u{feff}\u{1b}[2m2026-11-26T04:32:10.590339Z\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m \u{1b}[2mvictory_gpu::pipeline\u{1b}[0m\u{1b}[2m:\u{1b}[0m first bind \u{1b}[3mpipeline\u{1b}[0m\u{1b}[2m=\u{1b}[0m0x222cf05a070 \u{1b}[3mframe\u{1b}[0m\u{1b}[2m=\u{1b}[0m2 \u{1b}[3mstage\u{1b}[0m\u{1b}[2m=\u{1b}[0m\"vertex\" \u{1b}[3msize\u{1b}[0m\u{1b}[2m=\u{1b}[0m992 \u{1b}[3mhash\u{1b}[0m\u{1b}[2m=\u{1b}[0mdfafaf385b2f94e8";
        assert_eq!(clean(coloured), REAL);
    }

    #[test]
    fn quoted_fields_keep_their_spaces_and_escapes() {
        let rest = r#"host device facts adapter=Intel(R) Arc(TM) B580 switches="VICTORY_A=1 VICTORY_B=C:\\x \"y\"" wave_min=16"#;
        assert_eq!(
            field(rest, "switches").as_deref(),
            Some(r#"VICTORY_A=1 VICTORY_B=C:\x "y""#)
        );
        assert_eq!(field(rest, "wave_min").as_deref(), Some("16"));
        assert_eq!(field(rest, "min"), None);
    }

    #[test]
    fn the_numbers_of_an_each_list() {
        let rest = "frame times: 3 frames in 5.0 s: shortest 1 ms, median 33, longest 82; 0 long (over 73 ms, the usual frame of the last 64 being 33); each: 34 33 82";
        assert_eq!(each(rest, "frame times:"), Some(vec![34.0, 33.0, 82.0]));
        assert_eq!(each(rest, "frame draws:"), None);
    }
}
