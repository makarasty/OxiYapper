//! The only module allowed to touch the network (ARCHITECTURE 12).
//!
//! Model download: `<file>.part` with `Range` resume, SHA256 while streaming, rename on
//! match, `.part` removed on mismatch. Every URL, redirects included, passes the host
//! allowlist before a request is made.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use oxi_core::models::{ModelEntry, hash_file, hex, sha256_file, stamp_file_name, write_stamp};
use oxi_core::paths::part_path;
use sha2::{Digest, Sha256};
use ureq::Agent;
use ureq::http::Uri;

const MAX_REDIRECTS: usize = 5;
const CHUNK: usize = 64 * 1024;

/// Exact hosts. Subdomains of `SUFFIXES` are allowed too (HF CDN hosts change, e.g.
/// `cdn-lfs-us-1.hf.co`).
const HOSTS: &[&str] = &[
    "huggingface.co",
    "hf.co",
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];
const SUFFIXES: &[&str] = &[".huggingface.co", ".hf.co"];

/// Whether `host` may be contacted in production.
fn host_allowed(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    HOSTS.contains(&host.as_str()) || SUFFIXES.iter().any(|s| host.ends_with(s))
}

/// Which URLs a download may touch. Production is fixed; tests build their own to reach a
/// fake server on 127.0.0.1 without opening the real allowlist.
#[derive(Clone, Copy)]
struct Policy {
    host_ok: fn(&str) -> bool,
    https_only: bool,
    /// Longest silence allowed between two reads; a stalled server fails instead of hanging.
    read_timeout: Duration,
}

impl Policy {
    const PRODUCTION: Self = Self {
        host_ok: host_allowed,
        https_only: true,
        read_timeout: Duration::from_secs(30),
    };

    fn check(&self, uri: &Uri) -> Result<(), DownloadError> {
        let host = uri.host().unwrap_or_default();
        let scheme_ok = match uri.scheme_str() {
            Some("https") => true,
            Some("http") => !self.https_only,
            _ => false,
        };
        if scheme_ok && (self.host_ok)(host) {
            Ok(())
        } else {
            Err(DownloadError::Refused(uri.to_string()))
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    /// URL (or redirect target) outside the allowlist; nothing was requested.
    #[error("host not allowed: {0}")]
    Refused(String),
    /// Finished file does not match the registry; `.part` was removed.
    #[error("hash mismatch: expected {expected}, got {actual}")]
    Hash { expected: String, actual: String },
    /// More bytes than the registry size; `.part` was removed.
    #[error("server sent more bytes than the model size")]
    TooLarge,
    #[error("download cancelled")]
    Cancelled,
    /// Status that is neither success nor a resumable condition.
    #[error("server answered HTTP {0}")]
    Http(u16),
    /// Connection failed, stalled or dropped; `.part` is kept for resume.
    #[error("network error")]
    Network(#[source] io::Error),
    /// Local file error (disk full included); `.part` was removed on a write failure.
    #[error("file error")]
    Io(#[from] io::Error),
}

fn network(msg: &str) -> DownloadError {
    DownloadError::Network(io::Error::other(msg))
}

/// What to do with a response to a request that asked to resume at `have` bytes.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// 206 starting at `have`: append to `.part`.
    Append,
    /// 200: the body is the whole file, so `.part` is overwritten (Range was ignored).
    Truncate,
    /// The server answered a different range or 416: drop `.part`, ask again from zero.
    Discard,
}

fn plan(have: u64, status: u16, content_range: Option<&str>) -> Result<Plan, u16> {
    match status {
        200 => Ok(Plan::Truncate),
        206 if have > 0 => match content_range_start(content_range) {
            Some(start) if start == have => Ok(Plan::Append),
            _ => Ok(Plan::Discard),
        },
        416 if have > 0 => Ok(Plan::Discard),
        s => Err(s),
    }
}

/// First byte of `bytes 100-999/1000`.
fn content_range_start(header: Option<&str>) -> Option<u64> {
    header?
        .strip_prefix("bytes ")?
        .split('-')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// A verified model in place.
#[derive(Debug)]
#[cfg_attr(
    not(any(test, windows)),
    expect(dead_code, reason = "only the Windows daemon downloads for now")
)]
pub struct Downloaded {
    pub path: PathBuf,
    /// Why `<file>.ok` could not be written (any stale one was removed if possible). The
    /// model is fine; the next load only re-hashes it.
    pub stamp_error: Option<io::Error>,
}

/// Download `entry` into `dest_dir` and return the verified file. `progress` gets
/// `(done, total)` bytes; `cancel` stops the transfer and keeps `.part` for a later resume.
#[cfg_attr(
    not(any(test, windows)),
    expect(dead_code, reason = "only the Windows daemon downloads for now")
)]
pub fn download(
    entry: &ModelEntry,
    dest_dir: &Path,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<Downloaded, DownloadError> {
    download_with(&Policy::PRODUCTION, entry, dest_dir, progress, cancel)
}

fn download_with(
    policy: &Policy,
    entry: &ModelEntry,
    dest_dir: &Path,
    progress: &mut dyn FnMut(u64, u64),
    cancel: &AtomicBool,
) -> Result<Downloaded, DownloadError> {
    fs::create_dir_all(dest_dir)?;
    let dest = dest_dir.join(entry.file_name);
    let part = part_path(&dest);
    let agent = Agent::new_with_config(
        Agent::config_builder()
            .max_redirects(0)
            .max_redirects_will_error(false)
            .http_status_as_error(false)
            .https_only(policy.https_only)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            .timeout_per_read(Some(policy.read_timeout))
            .build(),
    );

    let mut have = fs::metadata(&part).map_or(0, |m| m.len());
    if have > entry.size {
        fs::remove_file(&part)?;
        have = 0;
    }
    loop {
        if have == entry.size {
            // A complete `.part` (interrupted before the rename): verify, no request.
            return finish(sha256_file(&part)?, &part, &dest, entry);
        }
        let mut resp = fetch(&agent, policy, entry.url, have)?;
        let status = resp.status().as_u16();
        let range = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok());
        let mut hasher = Sha256::new();
        let (mut file, mut done) = match plan(have, status, range).map_err(DownloadError::Http)? {
            Plan::Discard => {
                fs::remove_file(&part)?;
                have = 0;
                continue;
            }
            Plan::Append => {
                hasher = hash_file(&part)?;
                (OpenOptions::new().append(true).open(&part)?, have)
            }
            Plan::Truncate => (File::create(&part)?, 0),
        };

        let mut reader = resp.body_mut().as_reader();
        let mut buf = vec![0u8; CHUNK];
        // Read to the end of the body: a read can overshoot the registry size by a whole chunk.
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(DownloadError::Cancelled);
            }
            let n = reader.read(&mut buf).map_err(DownloadError::Network)?;
            if n == 0 {
                break;
            }
            if done + n as u64 > entry.size {
                drop(file);
                let _ = fs::remove_file(&part);
                return Err(DownloadError::TooLarge);
            }
            if let Err(e) = file.write_all(&buf[..n]) {
                drop(file);
                let _ = fs::remove_file(&part);
                return Err(e.into());
            }
            hasher.update(&buf[..n]);
            done += n as u64;
            progress(done, entry.size);
        }
        if done < entry.size {
            return Err(network("connection closed early"));
        }
        // On disk before the rename, so a crash cannot leave a verified name on lost data.
        file.sync_all()?;
        drop(file);
        return finish(hex(&hasher.finalize()), &part, &dest, entry);
    }
}

/// Compare the hex digest with the registry: rename and stamp on match, delete `.part` on
/// mismatch.
fn finish(
    actual: String,
    part: &Path,
    dest: &Path,
    entry: &ModelEntry,
) -> Result<Downloaded, DownloadError> {
    if actual == entry.sha256 {
        fs::rename(part, dest)?;
        let name = dest.file_name().unwrap_or_default().to_string_lossy();
        let stamp = dest.with_file_name(stamp_file_name(&name));
        // Best effort: failing here after the rename would make a retry download it again.
        let stamp_error = write_stamp(dest).err();
        if stamp_error.is_some() {
            let _ = fs::remove_file(&stamp);
        }
        Ok(Downloaded {
            path: dest.to_path_buf(),
            stamp_error,
        })
    } else {
        let _ = fs::remove_file(part);
        Err(DownloadError::Hash {
            expected: entry.sha256.to_string(),
            actual,
        })
    }
}

/// GET with `Range: bytes=<have>-` (when `have > 0`), following redirects by hand so each
/// hop passes the policy before any request is sent to it.
fn fetch(
    agent: &Agent,
    policy: &Policy,
    url: &str,
    have: u64,
) -> Result<ureq::http::Response<ureq::Body>, DownloadError> {
    let mut uri: Uri = url
        .parse()
        .map_err(|_| DownloadError::Refused(url.to_string()))?;
    for _ in 0..=MAX_REDIRECTS {
        policy.check(&uri)?;
        let mut req = agent.get(uri.to_string());
        if have > 0 {
            req = req.header("Range", format!("bytes={have}-"));
        }
        let resp = req
            .call()
            .map_err(|e| DownloadError::Network(e.into_io()))?;
        if !matches!(resp.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            return Ok(resp);
        }
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| network("redirect without Location"))?;
        uri = resolve(&uri, location)?;
    }
    Err(network("too many redirects"))
}

/// Absolute, protocol-relative (`//host/x`) or root-relative `Location` against the
/// current URL.
fn resolve(base: &Uri, location: &str) -> Result<Uri, DownloadError> {
    let scheme = base.scheme_str().unwrap_or("https");
    let full = if location.starts_with("//") {
        format!("{scheme}:{location}")
    } else if location.starts_with('/') {
        let authority = base
            .authority()
            .map(ToString::to_string)
            .unwrap_or_default();
        format!("{scheme}://{authority}{location}")
    } else {
        location.to_string()
    };
    full.parse()
        .map_err(|_| DownloadError::Refused(location.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxi_core::models::Stamp;
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn allowlist_table() {
        let cases = [
            ("huggingface.co", true),
            ("HuggingFace.co", true),
            ("cdn-lfs.huggingface.co", true),
            ("cdn-lfs-us-1.hf.co", true),
            ("api.github.com", true),
            ("github.com", true),
            ("objects.githubusercontent.com", true),
            ("release-assets.githubusercontent.com", true),
            ("127.0.0.1", false),
            ("localhost", false),
            ("evilhuggingface.co", false),
            ("huggingface.co.evil.com", false),
            ("example.com", false),
            ("", false),
        ];
        for (host, want) in cases {
            assert_eq!(host_allowed(host), want, "{host}");
        }
    }

    #[test]
    fn plan_table() {
        let cr = Some("bytes 100-999/1000");
        let cases = [
            (0, 200, None, Ok(Plan::Truncate)),
            (100, 200, None, Ok(Plan::Truncate)),
            (100, 206, cr, Ok(Plan::Append)),
            (100, 206, Some("bytes 0-999/1000"), Ok(Plan::Discard)),
            (100, 206, None, Ok(Plan::Discard)),
            (100, 416, None, Ok(Plan::Discard)),
            (0, 416, None, Err(416)),
            (0, 206, cr, Err(206)),
            (0, 404, None, Err(404)),
            (100, 503, None, Err(503)),
        ];
        for (have, status, range, want) in cases {
            assert_eq!(plan(have, status, range), want, "{have} {status} {range:?}");
        }
    }

    #[test]
    fn content_range_start_table() {
        assert_eq!(content_range_start(Some("bytes 5-9/10")), Some(5));
        assert_eq!(content_range_start(Some("bytes */10")), None);
        assert_eq!(content_range_start(Some("items 5-9/10")), None);
        assert_eq!(content_range_start(None), None);
    }

    // ---- fake server on 127.0.0.1 -------------------------------------------------

    const LOCAL: Policy = Policy {
        host_ok: |h| h == "127.0.0.1",
        https_only: false,
        read_timeout: Duration::from_secs(5),
    };

    type Handler = Box<dyn Fn(Option<u64>) -> Vec<u8> + Send>;

    /// Serves one connection per handler; each gets the `Range` start the client asked
    /// for and returns the raw HTTP response. Returns the port and the connection count.
    fn serve(handlers: Vec<Handler>) -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&hits);
        std::thread::spawn(move || {
            for handler in handlers {
                let Ok((mut sock, _)) = listener.accept() else {
                    return;
                };
                seen.fetch_add(1, Ordering::SeqCst);
                let mut reader = io::BufReader::new(sock.try_clone().unwrap());
                let mut range = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        range = v.trim().trim_end_matches('-').parse().ok();
                    }
                }
                let _ = sock.write_all(&handler(range));
            }
        });
        (port, hits)
    }

    fn response(status: &str, extra: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn payload() -> Vec<u8> {
        (0..200_000u32).map(|i| (i % 251) as u8).collect()
    }

    /// Registry entry for `data` served from `port`. Leaks: entries hold `'static` strs.
    fn entry(port: u16, data: &[u8]) -> ModelEntry {
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        ModelEntry {
            id: "test",
            display_name: "test",
            file_name: "model.bin",
            url: leak(format!("http://127.0.0.1:{port}/model.bin")),
            size: data.len() as u64,
            sha256: leak(hex(&Sha256::digest(data))),
        }
    }

    fn run(entry: &ModelEntry, dir: &Path) -> Result<Downloaded, DownloadError> {
        download_with(&LOCAL, entry, dir, &mut |_, _| {}, &AtomicBool::new(false))
    }

    fn ok_handler(data: Vec<u8>) -> Handler {
        Box::new(move |_| response("200 OK", "", &data))
    }

    fn partial_handler(data: &[u8], from: usize) -> Handler {
        let tail = data[from..].to_vec();
        let total = data.len();
        Box::new(move |_| {
            let cr = format!("Content-Range: bytes {from}-{}/{total}\r\n", total - 1);
            response("206 Partial Content", &cr, &tail)
        })
    }

    fn tempdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oxi-net-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn downloads_and_verifies() {
        let data = payload();
        let (port, _) = serve(vec![ok_handler(data.clone())]);
        let dir = tempdir("ok");
        let mut last = (0, 0);
        let path = download_with(
            &LOCAL,
            &entry(port, &data),
            &dir,
            &mut |d, t| last = (d, t),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(fs::read(path.path).unwrap(), data);
        assert_eq!(last, (data.len() as u64, data.len() as u64));
        assert!(!dir.join("model.bin.part").exists());
        let stamp = Stamp::parse(&fs::read_to_string(dir.join("model.bin.ok")).unwrap()).unwrap();
        assert_eq!(stamp.size, data.len() as u64);
        assert!(stamp.mtime_ms > 0);
    }

    #[test]
    fn unwritable_stamp_still_returns_the_model() {
        let data = payload();
        let (port, _) = serve(vec![ok_handler(data.clone())]);
        let dir = tempdir("nostamp");
        fs::create_dir(dir.join("model.bin.ok")).unwrap();
        let got = run(&entry(port, &data), &dir).unwrap();
        assert!(got.stamp_error.is_some());
        assert_eq!(fs::read(got.path).unwrap(), data);
        assert!(!dir.join("model.bin.part").exists());
    }

    #[test]
    fn corrupted_payload_is_refused_without_part() {
        let data = payload();
        let mut bad = data.clone();
        bad[1000] ^= 0xff;
        let (port, _) = serve(vec![ok_handler(bad)]);
        let dir = tempdir("bad");
        let err = run(&entry(port, &data), &dir).unwrap_err();
        assert!(matches!(err, DownloadError::Hash { .. }), "{err}");
        assert!(!dir.join("model.bin.part").exists());
        assert!(!dir.join("model.bin").exists());
        assert!(!dir.join("model.bin.ok").exists());
    }

    #[test]
    fn resumes_at_the_part_length() {
        let data = payload();
        let cut = 70_000;
        let (port, _) = serve(vec![Box::new({
            let ok = partial_handler(&data, cut);
            move |range| {
                assert_eq!(range, Some(cut as u64));
                ok(range)
            }
        })]);
        let dir = tempdir("resume");
        fs::write(dir.join("model.bin.part"), &data[..cut]).unwrap();
        let path = run(&entry(port, &data), &dir).unwrap();
        assert_eq!(fs::read(path.path).unwrap(), data);
        assert!(!dir.join("model.bin.part").exists());
    }

    #[test]
    fn restarts_from_zero_when_range_is_ignored() {
        let data = payload();
        let (port, _) = serve(vec![ok_handler(data.clone())]);
        let dir = tempdir("norange");
        fs::write(dir.join("model.bin.part"), &data[..70_000]).unwrap();
        let path = run(&entry(port, &data), &dir).unwrap();
        assert_eq!(fs::read(path.path).unwrap(), data);
    }

    #[test]
    fn wrong_offset_answer_restarts_from_zero() {
        let data = payload();
        let (port, hits) = serve(vec![partial_handler(&data, 1000), ok_handler(data.clone())]);
        let dir = tempdir("offset");
        fs::write(dir.join("model.bin.part"), &data[..70_000]).unwrap();
        let path = run(&entry(port, &data), &dir).unwrap();
        assert_eq!(fs::read(path.path).unwrap(), data);
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn stale_part_with_wrong_bytes_fails_the_hash_and_is_removed() {
        let data = payload();
        let cut = 70_000;
        let (port, _) = serve(vec![partial_handler(&data, cut)]);
        let dir = tempdir("stale");
        fs::write(dir.join("model.bin.part"), vec![7u8; cut]).unwrap();
        let err = run(&entry(port, &data), &dir).unwrap_err();
        assert!(matches!(err, DownloadError::Hash { .. }), "{err}");
        assert!(!dir.join("model.bin.part").exists());
    }

    #[test]
    fn follows_allowed_redirect() {
        let data = payload();
        let (port, _) = serve(vec![
            Box::new(|_| response("302 Found", "Location: /real.bin\r\n", b"")),
            ok_handler(data.clone()),
        ]);
        let dir = tempdir("redir");
        assert!(run(&entry(port, &data), &dir).is_ok());
    }

    #[test]
    fn redirect_to_other_host_is_refused() {
        let data = payload();
        let (port, hits) = serve(vec![Box::new(|_| {
            response(
                "302 Found",
                "Location: http://example.com/model.bin\r\n",
                b"",
            )
        })]);
        let dir = tempdir("evil");
        let err = run(&entry(port, &data), &dir).unwrap_err();
        assert!(matches!(err, DownloadError::Refused(_)), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(!dir.join("model.bin.part").exists());
    }

    #[test]
    fn production_policy_refuses_loopback_before_any_request() {
        let data = payload();
        let (port, hits) = serve(vec![ok_handler(data.clone())]);
        let dir = tempdir("prod");
        let err = download(
            &entry(port, &data),
            &dir,
            &mut |_, _| {},
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(matches!(err, DownloadError::Refused(_)), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn cancel_keeps_the_part() {
        let data = payload();
        let (port, _) = serve(vec![ok_handler(data.clone())]);
        let dir = tempdir("cancel");
        let cancel = AtomicBool::new(false);
        let err = download_with(
            &LOCAL,
            &entry(port, &data),
            &dir,
            &mut |_, _| cancel.store(true, Ordering::Relaxed),
            &cancel,
        )
        .unwrap_err();
        assert!(matches!(err, DownloadError::Cancelled), "{err}");
        let kept = fs::metadata(dir.join("model.bin.part")).unwrap().len();
        assert!(kept > 0 && kept < data.len() as u64);
    }

    #[test]
    fn resolve_table() {
        let base: Uri = "https://huggingface.co/a/b.bin".parse().unwrap();
        let cases = [
            ("https://cdn-lfs.hf.co/x", "cdn-lfs.hf.co", "/x"),
            ("//cdn-lfs.hf.co/x", "cdn-lfs.hf.co", "/x"),
            ("//example.com/x", "example.com", "/x"),
            ("/real.bin", "huggingface.co", "/real.bin"),
        ];
        for (location, host, path) in cases {
            let uri = resolve(&base, location).unwrap();
            assert_eq!(uri.scheme_str(), Some("https"), "{location}");
            assert_eq!(uri.host(), Some(host), "{location}");
            assert_eq!(uri.path(), path, "{location}");
        }
    }

    #[test]
    fn oversized_body_is_refused_without_part() {
        let data = payload();
        let mut big = data.clone();
        big.push(0);
        let (port, _) = serve(vec![ok_handler(big)]);
        let dir = tempdir("big");
        let err = run(&entry(port, &data), &dir).unwrap_err();
        assert!(matches!(err, DownloadError::TooLarge), "{err}");
        assert!(!dir.join("model.bin.part").exists());
        assert!(!dir.join("model.bin").exists());
    }

    #[test]
    fn stalled_body_times_out_and_keeps_the_part() {
        let data = payload();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let half = data[..100_000].to_vec();
        let total = data.len();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {total}\r\n\r\n");
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(&half);
            // Stall: hold the socket open until the client gives up and closes it.
            let mut sink = [0u8; 1024];
            while sock.read(&mut sink).is_ok_and(|n| n > 0) {}
        });
        let dir = tempdir("stall");
        let policy = Policy {
            read_timeout: Duration::from_millis(300),
            ..LOCAL
        };
        let entry = entry(port, &data);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_dir = dir.clone();
        std::thread::spawn(move || {
            let r = download_with(
                &policy,
                &entry,
                &thread_dir,
                &mut |_, _| {},
                &AtomicBool::new(false),
            );
            let _ = tx.send(r);
        });
        let err = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("download hung on a stalled server")
            .unwrap_err();
        assert!(matches!(err, DownloadError::Network(_)), "{err}");
        assert!(dir.join("model.bin.part").exists());
    }

    #[test]
    fn http_error_is_reported() {
        let data = payload();
        let (port, _) = serve(vec![Box::new(|_| response("404 Not Found", "", b""))]);
        let err = run(&entry(port, &data), &tempdir("404")).unwrap_err();
        assert!(matches!(err, DownloadError::Http(404)), "{err}");
    }
}
