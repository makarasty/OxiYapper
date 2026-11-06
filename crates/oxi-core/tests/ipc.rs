use oxi_core::config::OverlayPosition;
use oxi_core::device::{Device, DeviceChoice};
use oxi_core::hotkey::Key;
use oxi_core::ipc::{
    DecodeError, Decoder, DownloadResult, MAX_LINE, PROTO, Push, Request, encode, pipe_name,
    socket_file,
};
use oxi_core::machine::Phase;

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
    assert_eq!(pipe_name("S-1-5-21-1"), r"\.\pipe\OxiYapper-S-1-5-21-1");
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
