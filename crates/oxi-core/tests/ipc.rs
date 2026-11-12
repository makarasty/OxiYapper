use oxi_core::config::OverlayPosition;
use oxi_core::device::{Device, DeviceChoice};
use oxi_core::hotkey::Key;
use oxi_core::ipc::{
    DecodeError, Decoder, DownloadResult, HistoryItem, MAX_LINE, PROTO, Push, Request, Spoken,
    TestFailure, TestOutcome, WatchedFile, encode, pipe_name, socket_file,
};
use oxi_core::machine::Phase;
use oxi_core::stats::{Date, Summary};
use oxi_core::tuning::SettingsSet;
use oxi_core::types::Language;

fn requests() -> Vec<Request> {
    vec![
        Request::Hello {
            proto: PROTO,
            version: "0.1.0".into(),
        },
        Request::OpenSettings,
        Request::Status,
        Request::ListDevices,
        Request::DownloadModel { id: "turbo".into() },
        Request::CancelDownload { id: "turbo".into() },
        Request::CaptureHotkey { on: true },
        Request::PreviewOverlay {
            position: OverlayPosition::TopCentre,
        },
        Request::Stats,
        Request::PreviewSound {
            choice: "pop".into(),
            volume_percent: 40,
        },
        Request::TestAudio {
            reset: true,
            pcm: "3f000000".into(),
        },
        Request::TestTranscribe {
            id: 7,
            set: SettingsSet::defaults(),
            language: Language::RU,
        },
        Request::Shutdown,
    ]
}

fn pushes() -> Vec<Push> {
    vec![
        Push::HelloAck {
            proto: PROTO,
            version: "0.1.0".into(),
        },
        Push::Raise,
        Push::Status {
            phase: Phase::Recording,
            model: "turbo".into(),
            device: DeviceChoice::Named("Vulkan0".into()),
            backend: "Vulkan0".into(),
            loaded: true,
            warnings: vec!["gpu not found".into()],
        },
        Push::Devices {
            mics: vec!["Mic".into()],
            gpus: vec![Device {
                name: "Vulkan0".into(),
                description: "Test GPU".into(),
                uma: false,
            }],
        },
        Push::DownloadProgress {
            id: "turbo".into(),
            done: 10,
            total: 100,
        },
        Push::DownloadDone {
            id: "turbo".into(),
            result: DownloadResult::Ok,
        },
        Push::DownloadDone {
            id: "tiny".into(),
            result: DownloadResult::Cancelled,
        },
        Push::DownloadDone {
            id: "tiny".into(),
            result: DownloadResult::Failed {
                message: "hash mismatch".into(),
            },
        },
        Push::Key {
            code: Key::F13,
            down: true,
        },
        Push::ConfigError {
            file: "config.toml".into(),
            line: 3,
            message: "bad".into(),
        },
        Push::Stats(Summary {
            dictations: 11_300,
            words: 299_900,
            wpm: 146,
            streak: 6,
            longest_streak: 40,
            heat_start: Date::from_ymd(2026, 11, 2).unwrap(),
            heat: vec![0, 1, 4, 2, 3, 0],
        }),
        Push::TestProgress {
            id: 7,
            percent: 0,
            est_ms: Some(1200),
        },
        Push::TestResult {
            id: 7,
            outcome: TestOutcome::Heard {
                text: "test".into(),
                ms: 840,
            },
        },
        Push::TestResult {
            id: 8,
            outcome: TestOutcome::Failed {
                why: TestFailure::ModelMissing,
            },
        },
        Push::HistoryAdded {
            item: HistoryItem {
                id: "2026-11-11/101500-123".into(),
                status: oxi_core::history::Status::Done,
                duration_ms: 2500,
                model: "turbo".into(),
                language: "auto".into(),
                text: Spoken("test".into()),
                held: Vec::new(),
            },
        },
        Push::HistoryRemoved {
            ids: vec![
                "2026-11-01/090000-000".into(),
                "2026-11-01/090100-000".into(),
            ],
        },
        Push::FileChanged {
            file: WatchedFile::Replace,
        },
        Push::Woke,
    ]
}

fn round_trip<T>(msgs: &[T])
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    for m in msgs {
        let line = encode(m);
        assert!(
            line.ends_with('\n') && line.matches('\n').count() == 1,
            "{line:?}"
        );
        let mut d = Decoder::new();
        d.push(line.as_bytes());
        assert_eq!(&d.decode::<T>().unwrap().unwrap(), m);
        assert!(d.decode::<T>().is_none());
    }
}

#[test]
fn every_message_round_trips() {
    round_trip(&requests());
    round_trip(&pushes());
}

#[test]
fn wire_shape_snapshots() {
    let lines = |v: Vec<String>| v.join("");
    insta::assert_snapshot!("requests", lines(requests().iter().map(encode).collect()));
    insta::assert_snapshot!("pushes", lines(pushes().iter().map(encode).collect()));
}

#[test]
fn names_come_from_app_name() {
    assert_eq!(pipe_name("S-1-5-21-1"), r"\\.\pipe\OxiYapper-S-1-5-21-1");
    assert_eq!(socket_file(), "OxiYapper.sock");
}

#[test]
fn unknown_optional_field_is_accepted() {
    let mut d = Decoder::new();
    d.push(br#"{"t":"CaptureHotkey","on":false,"future":[1,2]}"#);
    d.push(b"\n");
    assert_eq!(
        d.decode::<Request>().unwrap().unwrap(),
        Request::CaptureHotkey { on: false }
    );
}

#[test]
fn a_progress_push_without_an_estimate_still_decodes() {
    let mut d = Decoder::new();
    d.push(b"{\"t\":\"TestProgress\",\"id\":3,\"percent\":40}\n");
    assert_eq!(
        d.decode::<Push>().unwrap().unwrap(),
        Push::TestProgress {
            id: 3,
            percent: 40,
            est_ms: None
        }
    );
}

#[test]
fn unknown_tag_and_garbage_are_errors_and_decoder_goes_on() {
    let mut d = Decoder::new();
    d.push(b"{\"t\":\"Nope\"}\nnot json\n\xff\xfe\n\n");
    d.push(encode(&Request::Status).as_bytes());
    for _ in 0..3 {
        assert!(matches!(
            d.decode::<Request>(),
            Some(Err(DecodeError::Json(_)))
        ));
    }
    assert_eq!(d.decode::<Request>().unwrap().unwrap(), Request::Status);
    assert!(d.decode::<Request>().is_none());
}

#[test]
fn messages_split_across_pushes_and_crlf() {
    let line = encode(&Request::ListDevices);
    let (a, b) = line.as_bytes().split_at(5);
    let mut d = Decoder::new();
    d.push(a);
    assert!(d.decode::<Request>().is_none());
    d.push(b);
    d.push(b"{\"t\":\"Status\"}\r\n");
    assert_eq!(
        d.decode::<Request>().unwrap().unwrap(),
        Request::ListDevices
    );
    assert_eq!(d.decode::<Request>().unwrap().unwrap(), Request::Status);
}

/// `{"t":"Status","pad":"aaa..."}` padded to exactly `len` bytes.
fn padded(len: usize) -> Vec<u8> {
    let head = br#"{"t":"Status","pad":""#;
    let mut v = head.to_vec();
    v.resize(len - 2, b'a');
    v.extend_from_slice(b"\"}");
    assert_eq!(v.len(), len);
    v
}

#[test]
fn line_of_max_len_passes_one_byte_more_is_refused() {
    let mut d = Decoder::new();
    d.push(&padded(MAX_LINE));
    d.push(b"\n");
    assert_eq!(d.decode::<Request>().unwrap().unwrap(), Request::Status);

    d.push(&padded(MAX_LINE + 1));
    d.push(b"\n");
    assert!(matches!(
        d.decode::<Request>(),
        Some(Err(DecodeError::TooLong))
    ));
    assert!(d.decode::<Request>().is_none());
}

#[test]
fn line_of_max_len_with_crlf_split_between_pushes_passes() {
    let mut d = Decoder::new();
    let mut first = padded(MAX_LINE);
    first.push(b'\r');
    d.push(&first);
    assert!(d.decode::<Request>().is_none());
    d.push(b"\n");
    assert_eq!(d.decode::<Request>().unwrap().unwrap(), Request::Status);
    assert!(d.decode::<Request>().is_none());
}

#[test]
fn oversized_line_without_newline_fails_early_and_resyncs() {
    let mut d = Decoder::new();
    let long = vec![b'x'; MAX_LINE + 1];
    d.push(&long);
    assert!(matches!(
        d.decode::<Request>(),
        Some(Err(DecodeError::TooLong))
    ));
    // more of the same line: dropped, no second error, memory stays bounded
    d.push(&long);
    assert!(d.decode::<Request>().is_none());
    d.push(b"tail\n");
    d.push(encode(&Request::Shutdown).as_bytes());
    assert_eq!(d.decode::<Request>().unwrap().unwrap(), Request::Shutdown);
}

#[test]
fn test_result_debug_never_shows_the_text() {
    let push = Push::TestResult {
        id: 1,
        outcome: TestOutcome::Heard {
            text: "secret words".into(),
            ms: 5,
        },
    };
    assert!(!format!("{push:?}").contains("secret"));
}
