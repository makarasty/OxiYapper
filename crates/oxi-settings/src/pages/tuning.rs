//! The Test page's pure helpers: what a recording shows (length, waveform, history label) and the
//! sums of a run.

use oxi_core::ipc::HistoryItem;
use oxi_core::tuning::WordErrors;

/// Bars in a recording's waveform.
pub const BARS: usize = 48;

/// `m:ss` of `ms`, rounded down to the second.
pub fn clock(ms: u64) -> String {
    let seconds = ms / 1000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// The loudest sample of each of `bars` equal slices, scaled so the loudest bar is 1; all zero for
/// silence or no samples.
pub fn peaks(samples: &[f32], bars: usize) -> Vec<f32> {
    if samples.is_empty() || bars == 0 {
        return vec![0.0; bars];
    }
    let peak = |slice: &[f32]| slice.iter().fold(0.0_f32, |m, s| m.max(s.abs()));
    let slices: Vec<f32> = (0..bars)
        .map(|i| peak(&samples[i * samples.len() / bars..(i + 1) * samples.len() / bars]))
        .collect();
    let top = slices.iter().copied().fold(0.0_f32, f32::max);
    if top < 1e-4 {
        return vec![0.0; bars];
    }
    slices.into_iter().map(|p| p / top).collect()
}

/// `2026-11-08 13:00:05` of the id `2026-11-08/130005-123`; the id itself when it has another form.
pub fn when(id: &str) -> String {
    match oxi_core::history::started(id) {
        Some(at) => at[..19].replacen('T', " ", 1),
        None => id.to_owned(),
    }
}

/// One line of the history picker: when, how long, and the start of its text.
pub fn history_label(item: &HistoryItem) -> String {
    const SHOWN: usize = 60;
    let text = item.text.0.trim();
    let start: String = text.chars().take(SHOWN).collect();
    let more = if text.chars().count() > SHOWN {
        "…"
    } else {
        ""
    };
    let when = when(&item.id);
    let length = clock(item.duration_ms);
    if start.is_empty() {
        format!("{when} · {length}")
    } else {
        format!("{when} · {length} · {start}{more}")
    }
}

/// The word errors of one set over every recording, once each one was heard; `None` while one is
/// missing or failed.
pub fn total(cells: &[Option<WordErrors>]) -> Option<WordErrors> {
    cells
        .iter()
        .try_fold(WordErrors::default(), |sum, cell| cell.map(|e| sum.plus(e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxi_core::history::{HistoryId, Status};
    use oxi_core::ipc::Spoken;

    #[test]
    fn clock_shows_minutes_and_seconds() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(1999), "0:01");
        assert_eq!(clock(61_000), "1:01");
        assert_eq!(clock(300_000), "5:00");
    }

    #[test]
    fn peaks_scale_to_the_loudest_slice() {
        let mut samples = vec![0.0_f32; 400];
        samples[50] = -0.5;
        samples[350] = 0.25;
        let p = peaks(&samples, 4);
        assert_eq!(p, [1.0, 0.0, 0.0, 0.5]);
        assert_eq!(peaks(&[], 3), [0.0; 3]);
        assert_eq!(peaks(&[0.0; 10], 2), [0.0; 2], "silence draws a flat line");
        assert_eq!(peaks(&[0.3; 3], 5).len(), 5, "more bars than samples");
    }

    #[test]
    fn a_history_line_names_when_how_long_and_what() {
        let item = |text: &str| HistoryItem {
            id: HistoryId::parse("2026-11-08/130005-123").unwrap(),
            status: Status::Done,
            duration_ms: 12_400,
            model: String::new(),
            language: String::new(),
            text: Spoken(text.into()),
            held: Vec::new(),
        };
        assert_eq!(
            history_label(&item(" hello ")),
            "2026-11-08 13:00:05 · 0:12 · hello"
        );
        assert_eq!(history_label(&item("")), "2026-11-08 13:00:05 · 0:12");
        let long = "a".repeat(61);
        assert!(history_label(&item(&long)).ends_with(&format!("{}…", "a".repeat(60))));
        assert_eq!(when("not-an-id"), "not-an-id");
    }

    #[test]
    fn a_total_needs_every_recording() {
        let e = |errors, words| Some(WordErrors { errors, words });
        assert_eq!(total(&[e(1, 4), e(0, 6)]), e(1, 10));
        assert_eq!(total(&[e(1, 4), None]), None);
        assert_eq!(total(&[]), e(0, 0));
    }
}
