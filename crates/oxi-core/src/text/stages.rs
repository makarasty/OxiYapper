//! The optional text stages of the pipeline (arch-plan 9.1 steps 4, 6, 7, 8). Each is a pure
//! function over text; `mod.rs` decides which run. Text only, no logging.

use super::{Seg, WordIn};
use crate::config::Snippet;

/// A word and the whitespace before it, so a stage can drop a word and keep a line break.
#[derive(Clone)]
struct Tok {
    ws: String,
    word: String,
}

fn tokens(text: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let (mut ws, mut word) = (String::new(), String::new());
    for c in text.chars() {
        if c.is_whitespace() {
            if !word.is_empty() {
                out.push(Tok {
                    ws: std::mem::take(&mut ws),
                    word: std::mem::take(&mut word),
                });
            }
            ws.push(c);
        } else {
            word.push(c);
        }
    }
    if !word.is_empty() {
        out.push(Tok { ws, word });
    }
    out
}

fn join(toks: &[Tok]) -> String {
    let mut out = String::new();
    for (i, t) in toks.iter().enumerate() {
        if i > 0 {
            out.push_str(&t.ws);
        }
        out.push_str(&t.word);
    }
    out
}

/// Lowercase, without punctuation at either end: what a filler or a command is compared by.
fn core(word: &str) -> String {
    word.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

fn is_ender(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '…')
}

/// Does the word end a sentence (closing quotes and brackets after the mark are looked through)?
fn ends_sentence(word: &str) -> bool {
    word.trim_end_matches(['"', '\'', '»', ')', '”', '’'])
        .chars()
        .next_back()
        .is_some_and(is_ender)
}

/// The first letter upper-cased (a quote or bracket in front is skipped).
fn capitalise(word: &str) -> String {
    let mut out = String::with_capacity(word.len());
    let mut done = false;
    for c in word.chars() {
        if !done && c.is_alphanumeric() {
            out.extend(c.to_uppercase());
            done = true;
        } else {
            out.push(c);
        }
    }
    out
}

fn drop_soft_end(word: &mut String) {
    let n = word.trim_end_matches([',', ';', ':']).len();
    word.truncate(n);
}

/// Step 4: join the kept segments. A gap of `sentence_ms` or more between two words ends the
/// sentence on the left and starts one on the right; `paragraph_ms` (0 = never) also breaks the
/// paragraph. Word times are used when a segment has one per word; else the segment's own times.
pub(super) fn join_with_pauses(
    parts: &[(String, &Seg<'_>)],
    sentence_ms: u32,
    paragraph_ms: u32,
) -> String {
    struct Unit {
        text: String,
        t0: Option<u64>,
        t1: Option<u64>,
    }
    let mut units: Vec<Unit> = Vec::new();
    for (text, seg) in parts {
        let words: Vec<&str> = text.split_whitespace().collect();
        let timed = |w: &WordIn<'_>| w.t0.zip(w.t1);
        if words.len() == seg.words.len() && seg.words.iter().all(|w| timed(w).is_some()) {
            units.extend(words.iter().zip(seg.words).map(|(t, w)| Unit {
                text: (*t).to_owned(),
                t0: w.t0.map(|m| m.0),
                t1: w.t1.map(|m| m.0),
            }));
        } else {
            units.push(Unit {
                text: text.clone(),
                t0: Some(seg.t0.0),
                t1: Some(seg.t1.0),
            });
        }
    }
    let (sentence, paragraph) = (u64::from(sentence_ms), u64::from(paragraph_ms));
    let mut out = String::new();
    let mut prev_end: Option<Option<u64>> = None;
    for u in units {
        let Some(end) = prev_end.replace(u.t1) else {
            out.push_str(&u.text);
            continue;
        };
        let gap = end.zip(u.t0).map(|(a, b)| b.saturating_sub(a));
        let para = paragraph > 0 && gap.is_some_and(|g| g >= paragraph);
        let long = para || gap.is_some_and(|g| g >= sentence);
        if long {
            drop_soft_end(&mut out);
            if !ends_sentence(&out) {
                out.push('.');
            }
        }
        out.push_str(if para { "\n\n" } else { " " });
        out.push_str(&if long { capitalise(&u.text) } else { u.text });
    }
    out
}

/// Step 6: remove whole words found in `fillers` (case-insensitive), with the comma that belonged
/// to them (`Ну, э, привет` is `Ну, привет`). A sentence mark on the filler moves to the word
/// before; a sentence start left lower-case is capitalised.
pub(super) fn drop_fillers(text: &str, fillers: &[String]) -> String {
    let set: Vec<String> = fillers
        .iter()
        .map(|f| core(f))
        .filter(|f| !f.is_empty())
        .collect();
    if set.is_empty() {
        return text.to_owned();
    }
    let mut out: Vec<Tok> = Vec::new();
    let (mut cap_next, mut carried_break) = (false, None::<String>);
    for mut t in tokens(text) {
        if set.contains(&core(&t.word)) {
            let at_start = t.ws.contains('\n') || out.last().is_none_or(|p| ends_sentence(&p.word));
            let mark = t
                .word
                .chars()
                .rev()
                .take_while(|c| !c.is_alphanumeric())
                .filter(|c| is_ender(*c))
                .last();
            if let (Some(mark), Some(prev)) = (mark, out.last_mut())
                && !ends_sentence(&prev.word)
            {
                drop_soft_end(&mut prev.word);
                prev.word.push(mark);
            }
            cap_next |= at_start;
            if t.ws.contains('\n') {
                carried_break = Some(t.ws);
            }
            continue;
        }
        if cap_next {
            t.word = capitalise(&t.word);
            cap_next = false;
        }
        if let Some(ws) = carried_break.take()
            && !t.ws.contains('\n')
        {
            t.ws = ws;
        }
        out.push(t);
    }
    join(&out)
}

#[derive(Clone, Copy)]
enum Cmd {
    Line,
    Paragraph,
    DeleteLast,
}

const COMMANDS: [(&[&str], Cmd); 7] = [
    (&["новая", "строка"], Cmd::Line),
    (&["новый", "абзац"], Cmd::Paragraph),
    (&["удали", "последнее"], Cmd::DeleteLast),
    (&["new", "line"], Cmd::Line),
    (&["new", "paragraph"], Cmd::Paragraph),
    (&["delete", "that"], Cmd::DeleteLast),
    (&["scratch", "that"], Cmd::DeleteLast),
];

/// The sentence before a `delete` command goes: tokens back to the one after a sentence mark or a
/// line break.
fn delete_last(out: &mut Vec<Tok>) {
    while let Some(t) = out.pop() {
        if t.ws.contains('\n') || out.last().is_none_or(|p| ends_sentence(&p.word)) {
            break;
        }
    }
}

/// Step 7: whole-phrase voice commands. Returns the text and how many line breaks the commands
/// left at its very end (0, 1 or 2): the caller decides whether a break may be pasted.
pub(super) fn voice_commands(text: &str) -> (String, usize) {
    let toks = tokens(text);
    let cores: Vec<String> = toks.iter().map(|t| core(&t.word)).collect();
    let mut out: Vec<Tok> = Vec::new();
    let (mut brk, mut cap_next) = (String::new(), false);
    let mut i = 0;
    while i < toks.len() {
        let hit = COMMANDS.iter().find(|(words, _)| {
            cores.len() - i >= words.len() && cores[i..].iter().zip(*words).all(|(a, b)| a == b)
        });
        let Some((words, cmd)) = hit else {
            let mut t = toks[i].clone();
            if !brk.is_empty() {
                t.ws = std::mem::take(&mut brk);
            }
            if std::mem::take(&mut cap_next) {
                t.word = capitalise(&t.word);
            }
            out.push(t);
            i += 1;
            continue;
        };
        i += words.len();
        match cmd {
            Cmd::Line | Cmd::Paragraph => {
                let para = matches!(cmd, Cmd::Paragraph);
                if let Some(prev) = out.last_mut() {
                    drop_soft_end(&mut prev.word);
                }
                brk.push_str(if para { "\n\n" } else { "\n" });
                brk.truncate(2);
                cap_next = para || out.last().is_some_and(|p| ends_sentence(&p.word));
            }
            Cmd::DeleteLast => {
                delete_last(&mut out);
                (brk, cap_next) = (String::new(), false);
            }
        }
    }
    (join(&out), brk.len())
}

/// Lowercase, single-spaced, without punctuation at either end.
fn phrase_key(s: &str) -> String {
    let spaced = s
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ");
    spaced
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_owned()
}

/// Step 8: the block of the snippet whose trigger is the whole `text`, if any.
pub(super) fn snippet<'a>(text: &str, snippets: &'a [Snippet]) -> Option<&'a str> {
    let key = phrase_key(text);
    if key.is_empty() {
        return None;
    }
    snippets
        .iter()
        .find(|s| phrase_key(&s.trigger) == key)
        .map(|s| s.text.as_str())
}
