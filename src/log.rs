//! Log setup with tracing and a rolling file (ARCHITECTURE sections 7 and 12).
// Nothing calls this until the daemon wiring lands.
#![allow(dead_code, reason = "used by the daemon wiring")]

use std::borrow::Cow;
use std::panic;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_appender::rolling::{Builder, Rotation};

const FILE_PREFIX: &str = "oxi";
const FILE_SUFFIX: &str = "log";
const FILES_KEPT: usize = 7;

/// Keep it alive for the whole run: dropping it flushes the log. The panic hook shares it,
/// because `panic = "abort"` skips every destructor.
pub struct Guard(Arc<Mutex<Option<WorkerGuard>>>);

impl Guard {
    fn flush(&self) {
        // Dropping the worker guard waits (up to its timeout) until the queue is written.
        drop(self.0.lock().unwrap_or_else(PoisonError::into_inner).take());
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Daily files `oxi.<date>.log` in `logs_dir`, the newest 7 kept, written off-thread. When the
/// directory cannot be used the log goes to stderr instead: a logging fault never stops the app.
pub fn init(logs_dir: &Path) -> Guard {
    let (writer, worker) = match Builder::new()
        .rotation(Rotation::DAILY)
        .filename_prefix(FILE_PREFIX)
        .filename_suffix(FILE_SUFFIX)
        .max_log_files(FILES_KEPT)
        .build(logs_dir)
    {
        Ok(appender) => tracing_appender::non_blocking(appender),
        Err(e) => {
            eprintln!("log files unavailable, logging to stderr: {e}");
            tracing_appender::non_blocking(std::io::stderr())
        }
    };
    install(writer);
    let guard = Guard(Arc::new(Mutex::new(Some(worker))));
    install_panic_hook(&guard);
    guard
}

fn install(writer: NonBlocking) {
    // Err only when a subscriber is already set (a second init): keep the first.
    let _ = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .try_init();
}

fn install_panic_hook(guard: &Guard) {
    let shared = Guard(Arc::clone(&guard.0));
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        tracing::error!("panic: {info}");
        shared.flush();
        previous(info);
    }));
}

/// Dictated text for a log line: the text itself only when `debug.log_text` is on, else its
/// length. The one place that decides; every log call with user text goes through it.
pub fn text_for_log(text: &str, log_text: bool) -> Cow<'_, str> {
    if log_text {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(format!("[{} chars]", text.chars().count()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_hidden_unless_log_text_is_on() {
        assert_eq!(text_for_log("секрет", true), "секрет");
        let hidden = text_for_log("секрет", false);
        assert_eq!(hidden, "[6 chars]");
        assert!(!hidden.contains("секрет"));
        assert_eq!(text_for_log("", false), "[0 chars]");
    }

    #[test]
    fn files_are_written_and_flushed_on_guard_drop() {
        let dir = std::env::temp_dir().join(format!("oxi-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let guard = init(&dir);
        tracing::info!("hello from the log test");
        drop(guard);
        let logged: String = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| std::fs::read_to_string(e.unwrap().path()).unwrap())
            .collect();
        assert!(logged.contains("hello from the log test"), "{logged:?}");
    }
}
