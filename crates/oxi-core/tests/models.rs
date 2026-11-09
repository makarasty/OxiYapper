use std::collections::HashSet;

use std::time::{Duration, SystemTime};

use oxi_core::constants::HOUR;
use oxi_core::models::{
    MODELS, ModelStatus, PART_KEEP, Stamp, find, find_by_file_name, part_expired, sha256_file,
    stamp_file_name, stamp_trusted, status, write_stamp,
};

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
fn file_names_resolve_to_their_entry_and_others_are_custom() {
    for m in MODELS {
        assert_eq!(find_by_file_name(m.file_name).map(|e| e.id), Some(m.id));
    }
    for other in ["", "turbo", "my-model.bin", "GGML-TINY.BIN"] {
        assert!(find_by_file_name(other).is_none(), "{other}");
    }
}

#[test]
fn sha256_file_is_lower_hex_of_the_bytes() {
    let path = std::env::temp_dir().join(format!("oxi-sha-{}", std::process::id()));
    std::fs::write(&path, b"abc").unwrap();
    let got = sha256_file(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    // FIPS 180-2 test vector for "abc".
    assert_eq!(
        got,
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert!(sha256_file(&path).is_err());
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

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("oxi-status-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A file of the registry size without writing its bytes (`set_len`; the content is never read).
fn sized_file(dir: &std::path::Path, name: &str, len: u64) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::File::create(&path).unwrap().set_len(len).unwrap();
    path
}

#[test]
fn status_follows_size_and_stamp() {
    let entry = find("tiny").unwrap();
    let dir = scratch("table");
    let stamp = dir.join(stamp_file_name(entry.file_name));

    assert_eq!(status(entry, &dir), ModelStatus::Missing);

    let file = sized_file(&dir, entry.file_name, 5);
    assert_eq!(status(entry, &dir), ModelStatus::Bad, "wrong size");

    sized_file(&dir, entry.file_name, entry.size);
    assert_eq!(status(entry, &dir), ModelStatus::NeedsHash, "no stamp");

    write_stamp(&file).unwrap();
    assert_eq!(status(entry, &dir), ModelStatus::Ready);

    std::fs::write(&stamp, "garbage").unwrap();
    assert_eq!(status(entry, &dir), ModelStatus::NeedsHash, "bad stamp");

    let now = Stamp::of(&file).unwrap();
    let old = Stamp {
        mtime_ms: now.mtime_ms - 1,
        ..now
    };
    std::fs::write(&stamp, old.to_line()).unwrap();
    assert_eq!(status(entry, &dir), ModelStatus::NeedsHash, "file changed");

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn write_stamp_needs_an_existing_file() {
    let dir = scratch("nostamp");
    assert!(write_stamp(&dir.join("absent.bin")).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn part_age_rule() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_hours(100 * 24);
    let table = [
        ("6d23h", now - PART_KEEP.checked_sub(HOUR).unwrap(), false),
        ("7d", now - PART_KEEP, true),
        ("8d", now - PART_KEEP - Duration::from_hours(24), true),
        ("future", now + HOUR, false),
    ];
    for (case, modified, want) in table {
        assert_eq!(part_expired(modified, now), want, "{case}");
    }
}
