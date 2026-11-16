//! The Text page's suggestions from the person's own dictations (hold item 41, candidates 6-9):
//! the newest history entries are read and counted on a worker thread when the page opens (again
//! once that read is a day old, or on "Check again"); what to suggest is decided in
//! `oxi_core::learn`. The texts stay in memory on the worker; only the few words and counts the
//! page shows come back, and nothing is logged. Nothing changes until Apply or Add.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::rc::{Rc, Weak as RcWeak};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use oxi_core::config::{Snippet, Text};
use oxi_core::dictionary::{BUDGET, token_count};
use oxi_core::history::{self, Meta};
use oxi_core::learn::{self, Count, Doubted, Habits, Phrase, Sample};
use oxi_core::text::{fillers_to_lines, snippets_to_lines};
use slint::{ComponentHandle, ModelRc, Timer, TimerMode, VecModel, Weak};

use super::super::recent;
use crate::generated::{DoubtRow, FileState, TextFile, TextState};
use crate::{AppWindow, Ctx, Page};

/// At most this many entry files are opened for one read, text or not.
const MAX_READ: usize = 400;
/// How often a running read is asked for its answer (only while one runs).
const POLL: Duration = Duration::from_millis(50);
/// The doubted words the list shows at once; the next come as these are added or skipped.
const SHOWN_WORDS: usize = 6;

/// The suggestion slots, in `TextState.suggestion`'s numbering.
const PERIOD: i32 = 0;
const SMALL: i32 = 1;
const FILLERS: i32 = 2;
const PHRASE: i32 = 3;
const CERTAINTY: i32 = 4;

/// What the history says, counted on the worker with the settings of that moment.
#[derive(Debug, Default, Clone, PartialEq)]
pub(super) struct Found {
    pub doubted: Vec<Doubted>,
    pub habits: Habits,
    pub fillers: Option<(Vec<String>, Count)>,
    pub phrase: Option<Phrase>,
    pub certainty: Option<(f32, Count)>,
}

/// The settings the count needs, copied for the worker.
struct Snapshot {
    known: HashSet<String>,
    fillers: Vec<String>,
    snippets: Vec<Snippet>,
    certainty: f32,
}

/// The word pieces `word` adds to `dictionary` as its last line, and the total then: the same
/// count the dictionary's footer and the prompt use.
pub(super) fn cost(dictionary: &str, word: &str) -> (usize, usize) {
    let mut lines: Vec<&str> = dictionary
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let now = token_count(&lines.join(", "));
    lines.push(word.trim());
    let after = token_count(&lines.join(", "));
    (after.saturating_sub(now), after)
}

/// What the list shows under a doubted word about its cost; the second is true past the limit.
fn cost_note(dictionary: &str, word: &str) -> (String, bool) {
    let (added, after) = cost(dictionary, word);
    let pieces = if added == 1 {
        "word piece"
    } else {
        "word pieces"
    };
    if after > BUDGET {
        (
            format!(
                "Adds {added} {pieces}: {after} of {BUDGET}, over the limit, Whisper would not get it"
            ),
            true,
        )
    } else {
        (format!("Adds {added} {pieces}: {after} of {BUDGET}"), false)
    }
}

/// The key of the suggestion in `slot` as `found` has it now: a changed phrase, filler list or
/// certainty is a new suggestion, so an old "Not now" does not hide it.
fn slot_key(slot: i32, found: &Found) -> String {
    let (kind, what) = match slot {
        PERIOD => ("period", String::new()),
        SMALL => ("small", String::new()),
        FILLERS => (
            "fillers",
            found
                .fillers
                .iter()
                .flat_map(|(w, _)| w)
                .cloned()
                .collect::<Vec<_>>()
                .join(","),
        ),
        PHRASE => (
            "phrase",
            found.phrase.iter().map(|p| p.text.clone()).collect(),
        ),
        _ => (
            "certainty",
            found
                .certainty
                .map_or_else(String::new, |(v, _)| format!("{v:.2}")),
        ),
    };
    learn::dismiss_key(kind, &what)
}

fn word_key(word: &str) -> String {
    learn::dismiss_key("word", word)
}

/// The lower-case words of the dictionary and of both sides of the replacements: a doubted word
/// already there is no suggestion.
pub(super) fn known_words(dictionary: &str, replacements: &str) -> HashSet<String> {
    dictionary
        .lines()
        .chain(replacements.lines().flat_map(|l| l.split("=>")))
        .flat_map(str::split_whitespace)
        .map(|w| {
            w.trim_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase()
        })
        .filter(|w| !w.is_empty())
        .collect()
}

/// The samples of the newest entries with a text in the history folder `dir`, newest first.
fn read(dir: &Path) -> Vec<Sample> {
    let mut out = Vec::new();
    recent::newest(dir, MAX_READ, |day, stem| {
        let meta = fs::read_to_string(day.join(format!("{stem}.json")))
            .ok()
            .and_then(|json| Meta::from_json(&json).ok());
        let text = fs::read_to_string(day.join(format!("{stem}.txt"))).unwrap_or_default();
        if let Some(meta) = meta.filter(|_| !text.trim().is_empty()) {
            out.push(Sample::new(meta, text));
        }
        out.len() < learn::RECENT
    });
    out
}

fn analyse(dir: &Path, snap: &Snapshot) -> Found {
    let samples = read(dir);
    Found {
        doubted: learn::doubted_words(&samples, &snap.known),
        habits: learn::habits(&samples),
        fillers: learn::deleted_fillers(&samples, &snap.fillers),
        phrase: learn::repeated_phrase(&samples, &snap.snippets),
        certainty: learn::certainty(&samples, snap.certainty),
    }
}

/// What the page shows of `found` with the settings as they are now.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Tips {
    pub period: String,
    pub small: String,
    pub fillers: String,
    pub phrase: String,
    pub certainty: String,
    /// The doubted words still to offer.
    pub doubted: Vec<Doubted>,
}

fn of_edits(c: Count) -> String {
    format!("{} of your last {} edits", c.hits, c.of)
}

/// The fillers of `found` the list does not have yet.
fn new_fillers<'a>(found: &'a Found, text: &Text) -> Vec<&'a String> {
    found
        .fillers
        .iter()
        .flat_map(|(words, _)| words)
        .filter(|w| !text.fillers.contains(w))
        .collect()
}

/// The phrase of `found` unless a shortcut has it or its trigger now.
fn new_phrase<'a>(found: &'a Found, text: &Text) -> Option<&'a Phrase> {
    found.phrase.as_ref().filter(|p| {
        let lower = p.text.to_lowercase();
        !text
            .snippets
            .iter()
            .any(|s| s.text.to_lowercase() == lower || s.trigger.to_lowercase() == p.trigger)
    })
}

/// The words of every suggestion in `found` for `text` as it is now; `known`: the dictionary's and
/// replacements' words now, `dismissed`: the keys given "Not now" or "Not this one". Switches
/// already on say nothing.
pub(super) fn tips(
    found: &Found,
    text: &Text,
    known: &HashSet<String>,
    dismissed: &HashSet<String>,
) -> Tips {
    let shown = |slot| !dismissed.contains(&slot_key(slot, found));
    let period = found
        .habits
        .no_final_period
        .filter(|_| shown(PERIOD) && !text.no_final_period)
        .map(|c| {
            format!(
                "You took away the full stop at the end in {}. Turn this on?",
                of_edits(c)
            )
        });
    let small = found
        .habits
        .small_first_letter
        .filter(|_| shown(SMALL) && !text.small_first_letter)
        .map(|c| {
            format!(
                "You made the first letter small in {}. Turn this on?",
                of_edits(c)
            )
        });
    let fresh = new_fillers(found, text);
    let fillers = found
        .fillers
        .as_ref()
        .filter(|_| shown(FILLERS) && !fresh.is_empty())
        .map(|(_, c)| {
            let words: Vec<&str> = fresh.iter().map(|w| w.as_str()).collect();
            format!(
                "You deleted \"{}\" by hand in {}. Add to the list and turn on Remove filler words?",
                words.join("\", \""),
                of_edits(*c)
            )
        });
    let phrase = new_phrase(found, text).filter(|_| shown(PHRASE)).map(|p| {
        format!(
            "You said \"{}\" in {} of your last {} dictations. Add a shortcut: say \"{}\" alone to get it?",
            p.text, p.count.hits, p.count.of, p.trigger
        )
    });
    let certainty = found
        .certainty
        .filter(|&(v, _)| shown(CERTAINTY) && v < text.confidence_min - 0.004)
        .map(|(v, c)| {
            format!(
                "You added back {} of the last {} parts held back. Lower this to {:.0} %?",
                c.hits,
                c.of,
                v * 100.0
            )
        });
    let doubted = found
        .doubted
        .iter()
        .filter(|d| {
            let k = d.word.to_lowercase();
            !known.contains(&k) && !dismissed.contains(&word_key(&k))
        })
        .cloned()
        .collect();
    Tips {
        period: period.unwrap_or_default(),
        small: small.unwrap_or_default(),
        fillers: fillers.unwrap_or_default(),
        phrase: phrase.unwrap_or_default(),
        certainty: certainty.unwrap_or_default(),
        doubted,
    }
}

pub(super) struct Learn {
    window: Weak<AppWindow>,
    ctx: Ctx,
    me: RcWeak<Learn>,
    found: RefCell<Option<Found>>,
    /// When the last read started: one a day old is done again when the page opens.
    read_at: Cell<Option<Instant>>,
    reading: RefCell<Option<Receiver<Found>>>,
    /// Asks the running read for its answer; stopped once it came.
    timer: Timer,
    /// Keys of the suggestions given "Not now" or "Not this one"; kept in [`recent::DISMISSED_FILE`].
    dismissed: RefCell<HashSet<String>>,
    /// The doubted words the list shows now, in its order.
    listed: RefCell<Vec<String>>,
}

/// Wires the suggestions and the two style switches; runs again after Reset to defaults.
pub(super) fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<TextState>();
    let text = ctx.config.borrow().config.text.clone();
    s.set_no_final_period(text.no_final_period);
    s.set_small_first_letter(text.small_first_letter);
    let learn = Rc::new_cyclic(|me| Learn {
        window: window.as_weak(),
        ctx: ctx.clone(),
        me: me.clone(),
        found: RefCell::new(None),
        read_at: Cell::new(None),
        reading: RefCell::new(None),
        timer: Timer::default(),
        dismissed: RefCell::new(recent::dismissed(&ctx.layout.data_dir)),
        listed: RefCell::new(Vec::new()),
    });
    learn.refresh();
    let l = Rc::clone(&learn);
    s.on_no_final_period_toggled(move |on| l.set(|t| t.no_final_period = on));
    let l = Rc::clone(&learn);
    s.on_small_first_letter_toggled(move |on| l.set(|t| t.small_first_letter = on));
    let l = Rc::clone(&learn);
    s.on_learn_shown(move || l.shown());
    let l = Rc::clone(&learn);
    s.on_recheck(move || l.read());
    let l = Rc::clone(&learn);
    s.on_suggestion(move |slot, apply| l.suggestion(slot, apply));
    let l = Rc::clone(&learn);
    s.on_doubt_added(move |i| l.add_word(i));
    s.on_doubt_skipped(move |i| learn.skip_word(i));
    // Reset to defaults rebinds while the page may be open.
    if window.get_page() == Page::Text {
        s.invoke_learn_shown();
    }
}

impl Learn {
    fn set(&self, edit: impl FnOnce(&mut Text)) {
        super::apply_text(&self.window, &self.ctx, edit);
        self.refresh();
    }

    /// The page opened: read the first time and once the last read is a day old.
    fn shown(&self) {
        let fresh = self.read_at.get().is_some_and(|at| {
            let ms = u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX);
            !recent::stale(0, ms)
        });
        if !fresh {
            self.read();
        }
    }

    /// Count the newest history entries on a thread; a read already running is not doubled.
    fn read(&self) {
        if self.reading.borrow().is_some() {
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let s = window.global::<TextState>();
        let config = self.ctx.config.borrow();
        let text = &config.config.text;
        let snap = Snapshot {
            known: known_words(&s.get_dictionary(), &s.get_replacements()),
            fillers: text.fillers.clone(),
            snippets: text.snippets.clone(),
            certainty: text.confidence_min,
        };
        let dir = history::folder(&config.config.history, &self.ctx.layout.history_dir());
        drop(config);
        let (tx, answer) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(analyse(&dir, &snap));
        });
        self.read_at.set(Some(Instant::now()));
        *self.reading.borrow_mut() = Some(answer);
        let me = self.me.clone();
        self.timer.start(TimerMode::Repeated, POLL, move || {
            if let Some(learn) = me.upgrade() {
                learn.take();
            }
        });
    }

    fn take(&self) {
        let answer = self.reading.borrow().as_ref().map(Receiver::try_recv);
        match answer {
            Some(Ok(found)) => {
                *self.found.borrow_mut() = Some(found);
                self.done();
                self.refresh();
            }
            Some(Err(TryRecvError::Disconnected)) | None => self.done(),
            Some(Err(TryRecvError::Empty)) => {}
        }
    }

    fn done(&self) {
        *self.reading.borrow_mut() = None;
        self.timer.stop();
    }

    /// Every suggestion's words for the settings and the editors as they are now.
    fn refresh(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let s = window.global::<TextState>();
        let found = self.found.borrow();
        let t = found.as_ref().map_or_else(Tips::default, |f| {
            tips(
                f,
                &self.ctx.config.borrow().config.text,
                &known_words(&s.get_dictionary(), &s.get_replacements()),
                &self.dismissed.borrow(),
            )
        });
        s.set_period_tip(t.period.into());
        s.set_small_tip(t.small.into());
        s.set_filler_tip(t.fillers.into());
        s.set_phrase_tip(t.phrase.into());
        s.set_certainty_tip(t.certainty.into());
        s.set_doubted_count(i32::try_from(t.doubted.len()).unwrap_or(i32::MAX));
        let dictionary = s.get_dictionary();
        let rows: Vec<DoubtRow> = t
            .doubted
            .iter()
            .take(SHOWN_WORDS)
            .map(|d| {
                let (cost, over) = cost_note(&dictionary, &d.word);
                DoubtRow {
                    word: d.word.as_str().into(),
                    note: format!("in {} dictations", d.count).into(),
                    cost: cost.into(),
                    over,
                }
            })
            .collect();
        *self.listed.borrow_mut() = rows.iter().map(|r| r.word.to_string()).collect();
        s.set_doubted(ModelRc::new(VecModel::from(rows)));
    }

    /// A suggestion's Apply or Not now.
    fn suggestion(&self, slot: i32, apply: bool) {
        if !apply {
            if let Some(found) = self.found.borrow().as_ref() {
                self.dismiss(slot_key(slot, found));
            }
            return;
        }
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let s = window.global::<TextState>();
        let Some(found) = self.found.borrow().clone() else {
            return;
        };
        let text = self.ctx.config.borrow().config.text.clone();
        match slot {
            PERIOD => {
                s.set_no_final_period(true);
                self.set(|t| t.no_final_period = true);
            }
            SMALL => {
                s.set_small_first_letter(true);
                self.set(|t| t.small_first_letter = true);
            }
            FILLERS => {
                let mut list = text.fillers.clone();
                list.extend(new_fillers(&found, &text).into_iter().cloned());
                s.set_fillers(fillers_to_lines(&list).into());
                s.set_filler_filter(true);
                self.set(|t| {
                    t.fillers = list;
                    t.filler_filter = true;
                });
            }
            PHRASE => {
                let Some(p) = new_phrase(&found, &text) else {
                    return;
                };
                let mut list = text.snippets.clone();
                list.push(Snippet {
                    trigger: p.trigger.clone(),
                    text: p.text.clone(),
                });
                s.set_snippets(snippets_to_lines(&list).into());
                self.set(|t| t.snippets = list);
            }
            CERTAINTY => {
                let Some((v, _)) = found.certainty else {
                    return;
                };
                s.set_confidence_pct(v * 100.0);
                self.set(|t| t.confidence_min = v);
            }
            _ => {}
        }
    }

    /// Add: the word becomes the dictionary's last line. A clean editor is saved at once; one
    /// with unsaved edits keeps them and gets the word too (its Save writes both).
    fn add_word(&self, i: i32) {
        let word = usize::try_from(i)
            .ok()
            .and_then(|i| self.listed.borrow().get(i).cloned());
        let (Some(word), Some(window)) = (word, self.window.upgrade()) else {
            return;
        };
        let s = window.global::<TextState>();
        let mut info = super::info(&s, TextFile::Dictionary);
        if matches!(info.state, FileState::NotText | FileState::Unreadable) || info.conflict {
            return;
        }
        let mut text = s.get_dictionary().to_string();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&word);
        s.set_dictionary(text.as_str().into());
        super::show(&s, TextFile::Dictionary, &text);
        let was_dirty = info.dirty;
        info.dirty = true;
        super::set_info(&s, TextFile::Dictionary, info);
        if !was_dirty {
            super::save(&self.window, &self.ctx.layout, TextFile::Dictionary, false);
        }
        self.refresh();
    }

    /// Remember `key` as dismissed, in memory and in the file, and show the page without it.
    fn dismiss(&self, key: String) {
        let new = !self.dismissed.borrow().contains(&key);
        if new {
            recent::add_dismissed(&self.ctx.layout.data_dir, &key);
            self.dismissed.borrow_mut().insert(key);
        }
        self.refresh();
    }

    /// Not this one: the word goes from the list for good (a file in the data folder).
    fn skip_word(&self, i: i32) {
        let word = usize::try_from(i)
            .ok()
            .and_then(|i| self.listed.borrow().get(i).cloned());
        if let Some(word) = word {
            self.dismiss(word_key(&word));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found() -> Found {
        Found {
            doubted: vec![
                Doubted {
                    word: "Zorblat".into(),
                    count: 4,
                },
                Doubted {
                    word: "Kvex".into(),
                    count: 3,
                },
            ],
            habits: Habits {
                no_final_period: Some(Count { hits: 7, of: 9 }),
                small_first_letter: None,
            },
            fillers: Some((vec!["ну".into(), "типа".into()], Count { hits: 5, of: 9 })),
            phrase: Some(Phrase {
                text: "Best regards, Alex".into(),
                trigger: "best regards".into(),
                count: Count { hits: 4, of: 20 },
            }),
            certainty: Some((0.3, Count { hits: 4, of: 10 })),
        }
    }

    #[test]
    fn tips_say_counts_and_go_once_the_setting_is_in() {
        let f = found();
        let mut text = Text::default();
        let none = HashSet::new();
        let t = tips(&f, &text, &none, &none);
        assert_eq!(
            t.period,
            "You took away the full stop at the end in 7 of your last 9 edits. Turn this on?"
        );
        assert_eq!(t.small, "");
        assert_eq!(
            t.fillers,
            "You deleted \"ну\", \"типа\" by hand in 5 of your last 9 edits. Add to the list and turn on Remove filler words?"
        );
        assert!(t.phrase.contains("\"best regards\""), "{}", t.phrase);
        assert_eq!(
            t.certainty,
            "You added back 4 of the last 10 parts held back. Lower this to 30 %?"
        );
        assert_eq!(t.doubted.len(), 2);

        text.no_final_period = true;
        text.fillers.push("ну".into());
        text.confidence_min = 0.3;
        text.snippets.push(Snippet {
            trigger: "sign".into(),
            text: "best regards, alex".into(),
        });
        let known = known_words("Zorblat\n", "");
        let skipped: HashSet<String> = [word_key("kvex")].into();
        let t = tips(&f, &text, &known, &skipped);
        assert_eq!(t.period, "", "the switch is on");
        assert!(
            t.fillers.starts_with("You deleted \"типа\" "),
            "{}",
            t.fillers
        );
        assert_eq!(t.phrase, "", "a shortcut has it");
        assert_eq!(t.certainty, "", "already that low");
        assert!(t.doubted.is_empty(), "in the dictionary, or skipped");
        let both: HashSet<String> = [PERIOD, FILLERS].map(|n| slot_key(n, &f)).into();
        let quiet = tips(&f, &Text::default(), &none, &both);
        assert_eq!((quiet.period.as_str(), quiet.fillers.as_str()), ("", ""));
        assert!(!quiet.phrase.is_empty(), "other slots stay");
        // A different phrase is a new suggestion: the old "Not now" does not hide it.
        let phrase_off: HashSet<String> = [slot_key(PHRASE, &f)].into();
        let mut other = found();
        other.phrase.as_mut().unwrap().text = "Kind regards, Alex".into();
        assert_eq!(tips(&f, &Text::default(), &none, &phrase_off).phrase, "");
        let shown = tips(&other, &Text::default(), &none, &phrase_off).phrase;
        assert_ne!(shown, "");
    }

    #[test]
    fn the_cost_of_a_word_is_shown_with_the_limit() {
        let (added, after) = cost("", "Zorblat");
        assert!(added >= 1 && added == after, "{added} {after}");
        let (more, total) = cost("Kvex\n\n", "Zorblat");
        assert_eq!(total, token_count("Kvex, Zorblat"));
        assert_eq!(more, total - token_count("Kvex"));
        let (note, over) = cost_note("", "Zorblat");
        assert!(!over && note.contains(&format!("of {BUDGET}")), "{note}");
        // A dictionary filled to the limit: the next word is over it, and the note says so.
        let full =
            "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu\n".repeat(30);
        let (note, over) = cost_note(&full, "Zorblat");
        assert!(over && note.contains("over the limit"), "{note}");
    }

    #[test]
    fn dismissals_read_back_from_the_file_text() {
        let k = [word_key("zorblat"), slot_key(PERIOD, &found())];
        let file = format!("{}\n\n  {}  \n", k[0], k[1]);
        assert_eq!(recent::parse_dismissed(&file), k.iter().cloned().collect());
    }

    #[test]
    fn known_words_take_both_sides_of_a_replacement() {
        let k = known_words("Oxi Yapper\n\nKubernetes", "wisper => Whisper\nbad line");
        for w in [
            "oxi",
            "yapper",
            "kubernetes",
            "wisper",
            "whisper",
            "bad",
            "line",
        ] {
            assert!(k.contains(w), "{w}");
        }
    }
}
