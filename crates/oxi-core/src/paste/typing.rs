//! Chunks of typed text: how many UTF-16 units one input call sends (arch-plan section 3.4).

const CR: u16 = b'\r' as u16;
const LF: u16 = b'\n' as u16;

fn is_high(unit: u16) -> bool {
    (0xD800..0xDC00).contains(&unit)
}

fn is_low(unit: u16) -> bool {
    (0xDC00..0xE000).contains(&unit)
}

/// Index after the chunk that starts at `from`: at most `max` units (a `max` of 0 counts as 1);
/// never between the two halves of a surrogate pair (a pair is one chunk even when `max` is 1);
/// a line break (`\r\n`, `\r` or `\n`) is a chunk of its own. `units.len()` at the end.
#[must_use]
pub fn chunk_end(units: &[u16], from: usize, max: usize) -> usize {
    let max = max.max(1);
    match units.get(from) {
        None => return units.len(),
        Some(&CR) if units.get(from + 1) == Some(&LF) => return from + 2,
        Some(&(CR | LF)) => return from + 1,
        Some(_) => {}
    }
    let mut end = from;
    while end < units.len() && end - from < max {
        let unit = units[end];
        if unit == CR || unit == LF {
            break;
        }
        let width = if is_high(unit) && units.get(end + 1).copied().is_some_and(is_low) {
            2
        } else {
            1
        };
        if end > from && end - from + width > max {
            break;
        }
        end += width;
    }
    end
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn chunks(units: &[u16], max: usize) -> Vec<Vec<u16>> {
        let mut out = Vec::new();
        let mut from = 0;
        while from < units.len() {
            let end = chunk_end(units, from, max);
            out.push(units[from..end].to_vec());
            from = end;
        }
        out
    }

    fn u(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn paste_chunks_follow_the_table() {
        let pair = u("\u{1F600}");
        let plain16 = u(&"a".repeat(16));
        assert_eq!(chunks(&plain16, 16), std::slice::from_ref(&plain16));
        // A pair across the border stays whole: the chunk ends before it.
        let mut across = u(&"a".repeat(15));
        across.extend(&pair);
        assert_eq!(chunks(&across, 16), [u(&"a".repeat(15)), pair.clone()]);
        assert_eq!(chunks(&pair, 1), std::slice::from_ref(&pair));
        assert_eq!(chunks(&u("ab\ncd"), 16), [u("ab"), u("\n"), u("cd")]);
        assert_eq!(chunks(&u("ab\r\ncd"), 16), [u("ab"), u("\r\n"), u("cd")]);
        assert_eq!(chunks(&u("\r\r\n\n"), 16), [u("\r"), u("\r\n"), u("\n")]);
        assert_eq!(chunks(&[], 16), Vec::<Vec<u16>>::new());
        assert_eq!(chunk_end(&[], 0, 16), 0);
        // Lone or reversed surrogates go one unit at a time, nothing dropped.
        let broken = [0xD83D, 0x61, 0xDE00, 0xD83D];
        assert_eq!(chunks(&broken, 1), [[0xD83D], [0x61], [0xDE00], [0xD83D]]);
        assert_eq!(chunks(&u("abc"), 0), [u("a"), u("b"), u("c")]);
    }

    proptest! {
        #[test]
        fn paste_chunks_cover_the_input_once_and_never_split_a_pair(
            units in prop::collection::vec(
                prop_oneof![Just(0xD83D_u16), Just(0xDE00), Just(CR), Just(LF), 0x20_u16..0x80],
                0..80,
            ),
            max in 1_usize..20,
        ) {
            let got = chunks(&units, max);
            prop_assert_eq!(got.concat(), units.clone());
            let mut at = 0;
            for chunk in &got {
                prop_assert!(!chunk.is_empty());
                let pair_inside = chunk.len() == 2 && is_high(chunk[0]) && is_low(chunk[1]);
                prop_assert!(chunk.len() <= max || pair_inside || chunk == &[CR, LF]);
                at += chunk.len();
                // No pair is cut at this border.
                if at < units.len() {
                    prop_assert!(!(is_high(units[at - 1]) && is_low(units[at])));
                }
            }
        }
    }
}
