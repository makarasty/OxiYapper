//! Main thread: tray, overlay and the Win32 message loop (ARCHITECTURE section 2).
//!
//! [`Ui::on_wake`] drains the coordinator's `UiCmd`s into the tray icon, its balloon and the
//! overlay pill. Whether the pill shows at all is the core's `visible`, inside `Overlay::show`.

use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use oxi_core::config::{self, Config};
use oxi_core::machine::OverlayState;
use oxi_platform::os::overlay::Overlay;
use oxi_platform::os::tray::{MainLoop, Waker};

use super::cmd::UiCmd;

/// How long `PreviewOverlay` shows the pill.
const PREVIEW: Duration = Duration::from_secs(2);

pub struct Ui {
    rx: Receiver<UiCmd>,
    overlay: Overlay<'static>,
    /// `Overlay::show` reads `overlay` and `audio.meter_sensitivity_db` of it.
    cfg: Config,
    /// The last state the coordinator asked for; a preview ends by showing it again.
    state: OverlayState,
    preview_until: Option<Instant>,
    waker: Waker,
}

impl Ui {
    /// `cfg` is the start config; later `UiCmd::Config` replace the parts the overlay reads.
    pub fn new(rx: Receiver<UiCmd>, overlay: Overlay<'static>, cfg: Config, waker: Waker) -> Self {
        Self {
            rx,
            overlay,
            cfg,
            state: OverlayState::Hidden,
            preview_until: None,
            waker,
        }
    }

    /// Runs on the UI thread for each wake: every queued command, then an ended preview.
    pub fn on_wake(&mut self, tray: &mut MainLoop) {
        while let Ok(cmd) = self.rx.try_recv() {
            self.apply(cmd, tray);
        }
        if preview_over(self.preview_until, Instant::now()) {
            self.preview_until = None;
            self.show(self.state);
        }
    }

    fn apply(&mut self, cmd: UiCmd, tray: &mut MainLoop) {
        match cmd {
            UiCmd::Overlay(state) => {
                // A real state replaces a preview at once.
                self.preview_until = None;
                self.state = state;
                self.show(state);
            }
            UiCmd::Tray(phase) => {
                if let Err(e) = tray.set_phase(phase) {
                    tracing::warn!("tray icon not updated: {e}");
                }
            }
            UiCmd::Badge(badge) => {
                if let Err(e) = tray.set_badge(badge) {
                    tracing::warn!("tray badge not updated: {e}");
                }
            }
            UiCmd::Notify(notice) => {
                if let Err(e) = tray.notify(notice) {
                    tracing::warn!(?notice, "tray balloon not shown: {e}");
                }
            }
            UiCmd::PreviewOverlay(position) => {
                let mut cfg = self.cfg.clone();
                cfg.overlay.position = position;
                cfg.overlay.enabled = true;
                cfg.overlay.hide_over_fullscreen = false;
                if let Err(e) = self.overlay.show(preview_state(self.state), &cfg) {
                    tracing::warn!("overlay preview failed: {e}");
                }
                self.preview_until = Some(Instant::now() + PREVIEW);
                let waker = self.waker.clone();
                let timer =
                    thread::Builder::new()
                        .name("overlay-preview".into())
                        .spawn(move || {
                            thread::sleep(PREVIEW);
                            let _ = waker.wake();
                        });
                if let Err(e) = timer {
                    tracing::warn!(kind = ?e.kind(), "preview timer not started");
                }
            }
            UiCmd::Config {
                overlay,
                meter_sensitivity_db,
            } => {
                take_config(&mut self.cfg, overlay, meter_sensitivity_db);
                if self.preview_until.is_none() {
                    self.show(self.state);
                }
            }
            UiCmd::Warnings(warnings) => {
                if let Err(e) = tray.set_warnings(&warnings) {
                    tracing::warn!("tray tooltip not updated: {e}");
                }
            }
            UiCmd::CanCopy => tray.set_copy_last(true),
        }
    }

    fn show(&mut self, state: OverlayState) {
        if let Err(e) = self.overlay.show(state, &self.cfg) {
            tracing::warn!("overlay not shown: {e}");
        }
    }
}

/// What the preview pill shows: the current state, or the recording look when hidden.
fn preview_state(current: OverlayState) -> OverlayState {
    match current {
        OverlayState::Hidden => OverlayState::Recording {
            locked: false,
            ending_soon: false,
        },
        other => other,
    }
}

/// Puts the parts of a new active config the overlay reads into `cfg`. The meter sensitivity is
/// read when a recording starts, so the next one meters with it.
fn take_config(cfg: &mut Config, overlay: config::Overlay, meter_sensitivity_db: f32) {
    cfg.overlay = overlay;
    cfg.audio.meter_sensitivity_db = meter_sensitivity_db;
}

fn preview_over(until: Option<Instant>, now: Instant) -> bool {
    until.is_some_and(|until| now >= until)
}

#[cfg(test)]
mod tests {
    use oxi_core::machine::Notice;

    use super::*;

    #[test]
    fn preview_shows_the_recording_look_only_when_hidden() {
        let recording = OverlayState::Recording {
            locked: false,
            ending_soon: false,
        };
        assert_eq!(preview_state(OverlayState::Hidden), recording);
        let text = OverlayState::Text(Notice::Cancelled);
        assert_eq!(preview_state(text), text);
    }

    #[test]
    fn a_new_config_replaces_the_overlay_and_the_meter_sensitivity() {
        let mut cfg = Config::default();
        let overlay = config::Overlay {
            enabled: !cfg.overlay.enabled,
            ..cfg.overlay.clone()
        };
        take_config(&mut cfg, overlay.clone(), -9.0);
        assert_eq!(cfg.overlay, overlay);
        assert!((cfg.audio.meter_sensitivity_db + 9.0).abs() < f32::EPSILON);
    }

    #[test]
    fn preview_ends_at_its_deadline() {
        let now = Instant::now();
        assert!(!preview_over(None, now));
        assert!(!preview_over(Some(now + PREVIEW), now));
        assert!(preview_over(Some(now), now));
    }
}
