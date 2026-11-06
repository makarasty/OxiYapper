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

/// Separator strength: sentence-final marks beat `,;:`; 0 for anything else.
fn separator_rank(c: char) -> u8 {
    match c {
        '.' | '!' | '?' | '\u{2026}' => 2,
        ',' | ';' | ':' => 1,
        _ => 0,
    }
}

/// `chars[i]` closes what precedes it, so no space goes before it. A straight quote closes
/// only when it touches the text before it; after a space or at the start it opens.
fn is_closer(chars: &[char], i: usize) -> bool {
    let c = chars[i];
    separator_rank(c) > 0
        || matches!(c, ')' | ']' | '}' | '\u{201D}' | '\u{BB}')
        || (matches!(c, '"' | '\'') && i > 0 && !chars[i - 1].is_whitespace())
}

/// `c` opens what follows it, so no space goes after it. A straight quote opens only after a
/// space, the start, or another opener.
fn is_opener(c: char, prev: Option<char>) -> bool {
    matches!(c, '(' | '[' | '{' | '\u{201C}' | '\u{AB}')
        || (matches!(c, '"' | '\'') && prev.is_none_or(|p| p.is_whitespace() || is_opener(p, None)))
}

/// The closer that pairs with opener `c`.
fn pair_of(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '\u{201C}' => Some('\u{201D}'),
        '\u{AB}' => Some('\u{BB}'),
        '"' | '\'' => Some(c),
        _ => None,
    }
}

/// Join the text around a deleted word: `out` holds what came before, `chars[i..]` what
/// follows. Only the spacing and separators touching the deleted word change. Returns the
/// index to continue from.
fn join_after_deletion(out: &mut String, chars: &[char], mut i: usize) -> usize {
    let skip_space = |mut i: usize| {
        while chars.get(i).is_some_and(|c| c.is_whitespace()) {
            i += 1;
        }
        i
    };
    out.truncate(out.trim_end().len());
    i = skip_space(i);
    let mut rev = out.chars().rev();
    let (last, prev) = (rev.next(), rev.next());
    // At the start or after an opener the deleted word takes its separators with it.
    if last.is_none_or(|c| is_opener(c, prev)) {
        while chars.get(i).is_some_and(|&c| separator_rank(c) > 0) {
            i = skip_space(i + 1);
        }
        // A pair left empty goes too, with the space it leaves.
        if let (Some(open), Some(&close)) = (last, chars.get(i))
            && pair_of(open) == Some(close)
            && is_closer(chars, i)
        {
            out.pop();
            return join_after_deletion(out, chars, i + 1);
        }
        return i;
    }
    let left = last.map_or(0, separator_rank);
    let mut dropped = false;
    while let Some(&next) = chars.get(i) {
        if !is_closer(chars, i) {
            out.push(' ');
            break;
        }
        let right = separator_rank(next);
        if left > 0 && right > 0 && right <= left {
            // Doubled separator: the left one is as strong, drop the right one.
            i = skip_space(i + 1);
            dropped = true;
            continue;
        }
        if left == 1 && !dropped {
            // A comma-like mark before a stronger mark or a closer goes.
            out.pop();
        }
        break;
    }
    i
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
                if wanted.is_empty() {
                    i = join_after_deletion(&mut out, &chars, i);
                }
            } else {
                out.push(chars[i]);
                i += 1;
            }
        }
        out
    }
}
