/// `replace.txt` rules: `heard => wanted`, literal, whole word, case-insensitive,
/// longest match first, one pass (a rule never rewrites another rule's output).
#[derive(Debug, Clone, Default)]
pub struct ReplaceRules {
    /// Case-folded `heard` chars and the replacement, longest `heard` first.
    rules: Vec<(Vec<char>, String)>,
}

/// Rules read from a file plus the 1-based numbers of lines that were ignored.
#[derive(Debug, Clone, Default)]
pub struct ParsedRules {
    pub rules: ReplaceRules,
    pub bad_lines: Vec<usize>,
}

// shortcut: first char of the lowercase mapping, so a few special letters (U+0130) fold
// imperfectly; upgrade to full case folding if a user rule needs one.
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn is_apostrophe(c: char) -> bool {
    c == '\'' || c == '\u{2019}'
}

/// The char before `i` continues a word: a word char, or an apostrophe between word chars.
fn word_before(chars: &[char], i: usize) -> bool {
    match i.checked_sub(1).map(|j| chars[j]) {
        Some(c) if is_word(c) => true,
        Some(c) if is_apostrophe(c) => i >= 2 && is_word(chars[i - 2]),
        _ => false,
    }
}

/// The char at `i` continues a word: a word char, or an apostrophe before a word char,
/// except the possessive `'s` ending a word, so the stem of `GitHub's` matches but not `don't`.
fn word_after(chars: &[char], i: usize) -> bool {
    match chars.get(i) {
        Some(&c) if is_word(c) => true,
        Some(&c) if is_apostrophe(c) => {
            let next_is_word = |j: usize| chars.get(j).is_some_and(|&n| is_word(n));
            let possessive =
                chars.get(i + 1).is_some_and(|&n| fold(n) == 's') && !next_is_word(i + 2);
            next_is_word(i + 1) && !possessive
        }
        _ => false,
    }
}

impl ReplaceRules {
    /// Parse a `replace.txt`. Blank lines are skipped; a line without `=>` or with an empty
    /// left side is ignored and its number reported. When two lines share `heard`, the first wins.
    pub fn parse(text: &str) -> ParsedRules {
        let mut rules: Vec<(Vec<char>, String)> = Vec::new();
        let mut bad_lines = Vec::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match line.split_once("=>") {
                Some((heard, wanted)) if !heard.trim().is_empty() => {
                    rules.push((
                        heard.trim().chars().map(fold).collect(),
                        wanted.trim().to_owned(),
                    ));
                }
                _ => bad_lines.push(i + 1),
            }
        }
        // Stable sort keeps file order among equal lengths, so the first duplicate wins.
        rules.sort_by_key(|(heard, _)| std::cmp::Reverse(heard.len()));
        ParsedRules {
            rules: Self { rules },
            bad_lines,
        }
    }

    pub fn apply(&self, text: &str) -> String {
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::with_capacity(text.len());
        let mut i = 0;
        while i < chars.len() {
            let hit = self.rules.iter().find(|(heard, _)| {
                let end = i + heard.len();
                end <= chars.len()
                    && chars[i..end].iter().zip(heard).all(|(&c, &h)| fold(c) == h)
                    // A word edge needs a boundary; an edge like the `+` of `c++` does not.
                    && !(is_word(heard[0]) && word_before(&chars, i))
                    && !(is_word(heard[heard.len() - 1]) && word_after(&chars, end))
            });
            if let Some((heard, wanted)) = hit {
                out.push_str(wanted);
                i += heard.len();
                // A deleted word must not leave two spaces or a space before punctuation.
                if wanted.is_empty() && out.ends_with(char::is_whitespace) {
                    match chars.get(i) {
                        Some(c) if c.is_whitespace() => i += 1,
                        Some(c) if ".,!?;:\u{2026}".contains(*c) => {
                            out.truncate(out.trim_end().len());
                        }
                        _ => {}
                    }
                }
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        out
    }
}
