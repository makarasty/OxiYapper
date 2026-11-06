use std::collections::HashSet;

use oxi_core::models::{MODELS, Stamp, find, stamp_file_name, stamp_trusted};

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[test]
fn urls_are_pinned_to_a_commit_and_end_in_the_file_name() {
    for m in MODELS {
        let rest = m
            .url
            .strip_prefix("https://huggingface.co/")
            .unwrap_or_else(|| panic!("{}: host", m.id));
        let (_repo, tail) = rest
            .split_once("/resolve/")
            .unwrap_or_else(|| panic!("{}: no /resolve/", m.id));
        let (commit, file) = tail
            .split_once('/')
            .unwrap_or_else(|| panic!("{}: no file", m.id));
        assert!(
            is_hex(commit, 40),
            "{}: commit {commit:?} is not 40 hex",
            m.id
        );
        assert_eq!(file, m.file_name, "{}", m.id);
        assert!(!m.url.contains("/main/"), "{}", m.id);
    }
}

#[test]
fn hashes_are_64_lowercase_hex_and_sizes_nonzero() {
    for m in MODELS {
        assert!(is_hex(m.sha256, 64), "{}: {}", m.id, m.sha256);
        assert!(m.size > 0, "{}", m.id);
    }
}

#[test]
fn ids_and_file_names_are_unique() {
    let ids: HashSet<_> = MODELS.iter().map(|m| m.id).collect();
    let files: HashSet<_> = MODELS.iter().map(|m| m.file_name).collect();
    assert_eq!(ids.len(), MODELS.len());
    assert_eq!(files.len(), MODELS.len());
}

#[test]
fn curated_ids_resolve_and_custom_does_not() {
    for id in ["turbo", "large-v3", "podlodka", "tiny"] {
        assert_eq!(find(id).map(|m| m.id), Some(id));
    }
    assert!(find("custom").is_none());
    assert!(find("").is_none());
}

#[test]
fn stamp_file_is_file_name_plus_ok() {
    assert_eq!(stamp_file_name("ggml-tiny.bin"), "ggml-tiny.bin.ok");
}

#[test]
fn stamp_line_round_trips() {
    let stamp = Stamp {
        size: 77_691_713,
        mtime_ms: 1_762_400_000_123,
    };
    assert_eq!(stamp.to_line(), "77691713 1762400000123\n");
    assert_eq!(Stamp::parse(&stamp.to_line()), Some(stamp));
    assert_eq!(Stamp::parse("77691713 1762400000123\r\n"), Some(stamp));
    assert_eq!(Stamp::parse("77691713 1762400000123  \n"), Some(stamp));
}

#[test]
fn stamp_garbage_is_rejected() {
    for text in [
        "",
        "\n",
        "77691713",
        "77691713 ",
        "77691713 x",
        "-1 5",
        "1 2 3",
        "1  2",
        " 1 2",
        "1,2",
        "ok",
    ] {
        assert_eq!(Stamp::parse(text), None, "{text:?}");
    }
}

#[test]
fn stamp_policy() {
    let entry = find("tiny").unwrap();
    let good = Stamp {
        size: entry.size,
        mtime_ms: 1_000,
    };
    let cases = [
        ("same size and mtime", Some(good), good, true),
        ("no stamp", None, good, false),
        (
            "mtime changed",
            Some(good),
            Stamp {
                mtime_ms: 1_001,
                ..good
            },
            false,
        ),
        (
            "size changed",
            Some(good),
            Stamp {
                size: entry.size + 1,
                ..good
            },
            false,
        ),
        (
            "stamp matches but size is not the registry size",
            Some(Stamp {
                size: 5,
                mtime_ms: 1_000,
            }),
            Stamp {
                size: 5,
                mtime_ms: 1_000,
            },
            false,
        ),
    ];
    for (name, stamp, actual, want) in cases {
        assert_eq!(stamp_trusted(stamp, actual, entry), want, "{name}");
    }
}
