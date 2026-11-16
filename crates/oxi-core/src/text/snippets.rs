//! Snippets and fillers as the Text page edits them: one `trigger => text` per line, one filler
//! per line. Pure; the page keeps the lines, the config keeps the values.

use crate::config::Snippet;

/// `\n` in the text of a line is a line break, `\\` a backslash.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace("\r\n", "\n")
        .replace('\n', "\\n")
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some(other) if other != '\\' => {
                out.push('\\');
                out.push(other);
            }
            // `\\` is one backslash; a lone one at the end stays.
            _ => out.push('\\'),
        }
    }
    out
}

/// The lines the editor shows for `snippets`.
pub fn snippets_to_lines(snippets: &[Snippet]) -> String {
    snippets
        .iter()
        .map(|s| format!("{} => {}", s.trigger, escape(&s.text)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The snippets of the editor's `lines`. A line without `=>`, or with an empty trigger or text,
/// is skipped: it is still being typed.
pub fn snippets_from_lines(lines: &str) -> Vec<Snippet> {
    lines
        .lines()
        .filter_map(|line| {
            let (trigger, text) = line.split_once("=>")?;
            let (trigger, text) = (trigger.trim(), unescape(text.trim()));
            (!trigger.is_empty() && !text.is_empty()).then(|| Snippet {
                trigger: trigger.to_owned(),
                text,
            })
        })
        .collect()
}

/// The lines the editor shows for `fillers`.
pub fn fillers_to_lines(fillers: &[String]) -> String {
    fillers.join("\n")
}

/// The fillers of the editor's `lines`: trimmed, lower-case, empty lines and repeats dropped.
pub fn fillers_from_lines(lines: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in lines.lines() {
        let word = line.trim().to_lowercase();
        if !word.is_empty() && !out.contains(&word) {
            out.push(word);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snip(trigger: &str, text: &str) -> Snippet {
        Snippet {
            trigger: trigger.into(),
            text: text.into(),
        }
    }

    #[test]
    fn snippet_lines_round_trip_with_breaks_and_backslashes() {
        let snippets = vec![
            snip("мой адрес", "ул. Ленина, 1\nкв. 5"),
            snip("path", "C:\\temp\\new"),
            snip("arrow", "a => b"),
        ];
        let lines = snippets_to_lines(&snippets);
        assert_eq!(lines.lines().count(), 3, "one line each: {lines}");
        assert_eq!(snippets_from_lines(&lines), snippets);
    }

    #[test]
    fn a_line_still_being_typed_is_skipped() {
        let got = snippets_from_lines("no arrow\n => empty trigger\ntrigger =>\nok => fine\n\n");
        assert_eq!(got, vec![snip("ok", "fine")]);
    }

    #[test]
    fn filler_lines_are_trimmed_lower_case_and_unique() {
        assert_eq!(
            fillers_from_lines(" Э \n\nUM\num\r\nэм"),
            ["э", "um", "эм"].map(String::from)
        );
        assert_eq!(fillers_from_lines(&fillers_to_lines(&["э".into()])), ["э"]);
    }
}
