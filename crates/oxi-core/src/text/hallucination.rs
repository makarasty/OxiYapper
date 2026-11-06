/// Phrases whisper invents on silence, shipped with the app.
const DEFAULT_LIST: &str = include_str!("../../data/hallucinations.txt");

/// Drops whole segments that match the shipped list or the user's extra lines.
#[derive(Debug, Clone)]
pub struct HallucinationFilter {
    exact: Vec<String>,
    /// Entries written with a trailing `*`: the phrase plus any words after it.
    prefixes: Vec<String>,
}

/// Lowercase, punctuation removed, spaces collapsed.
fn normalise(s: &str) -> String {
    let kept: String = s
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl HallucinationFilter {
    /// The shipped list plus `user_lines` (one phrase per line, `#` starts a comment line).
    pub fn new(user_lines: &str) -> Self {
        let mut filter = Self {
            exact: Vec::new(),
            prefixes: Vec::new(),
        };
        for line in DEFAULT_LIST.lines().chain(user_lines.lines()) {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (target, phrase) = match line.strip_suffix('*') {
                Some(p) => (&mut filter.prefixes, p),
                None => (&mut filter.exact, line),
            };
            let phrase = normalise(phrase);
            if !phrase.is_empty() {
                target.push(phrase);
            }
        }
        filter
    }

    pub fn is_hallucination(&self, segment: &str) -> bool {
        let s = normalise(segment);
        self.exact.contains(&s)
            || self.prefixes.iter().any(|p| {
                s.strip_prefix(p.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
            })
    }
}

impl Default for HallucinationFilter {
    fn default() -> Self {
        Self::new("")
    }
}
