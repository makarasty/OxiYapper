//! Main thread: tray, overlay and the Win32 message loop (ARCHITECTURE section 2).
//!
//! [`Ui::on_wake`] drains the coordinator's `UiCmd`s into the tray icon, its balloon and the
//! overlay pill. Whether the pill shows at all is the core's `visible`, inside `Overlay::show`.

use std::sync::mpsc::Receiver;
use std::thread;
use std::time::{Duration, Instant};

use oxi_core::config::{self, Config, Rgb};
use oxi_core::machine::OverlayState;
use oxi_core::overlay::{Rgba, seed};
use oxi_platform::os::overlay::{Overlay, Progress};
use oxi_platform::os::tray::{MainLoop, Waker};

use super::cmd::UiCmd;

/// How long `PreviewOverlay` shows the pill.
const PREVIEW: Duration = Duration::from_secs(2);

/// How long a cue pill ("held back", "saved to file") stays: the machine's `overlay_hide`.
const CUE: Duration = Duration::from_millis(1500);

pub struct Ui {
    rx: Receiver<UiCmd>,
    overlay: Overlay<'static>,
    /// `Overlay::show` reads `overlay` and `audio.meter_sensitivity_db` of it.
    cfg: Config,
    /// The last state the coordinator asked for; a preview ends by showing it again.
    state: OverlayState,
    preview_until: Option<Instant>,
    /// The transcription pill stays until its bar has run to the end (DS-26); the state the
    /// coordinator asked for meanwhile waits in `state`.
    hold_until: Option<Instant>,
    /// A cue pill is not a machine notice: it ends here, not by a machine `Hidden`.
    cue_until: Option<Instant>,
    /// What the coordinator asked for while a cue was up: shown when the cue ends.
    after_cue: Option<OverlayState>,
    progress: Progress,
    waker: Waker,
}

impl Ui {
    /// `cfg` is the start config; later `UiCmd::Config` replace the parts the overlay reads.
    pub fn new(rx: Receiver<UiCmd>, overlay: Overlay<'static>, cfg: Config, waker: Waker) -> Self {
        Self {
            rx,
            progress: overlay.progress(),
            overlay,
            cfg,
            state: OverlayState::Hidden,
            preview_until: None,
            hold_until: None,
            cue_until: None,
            after_cue: None,
            waker,
        }
    }

    /// Runs on the UI thread for each wake: every queued command, then an ended preview.
    pub fn on_wake(&mut self, tray: &mut MainLoop) {
        while let Ok(cmd) = self.rx.try_recv() {
            self.apply(cmd, tray);
        }
        let now = Instant::now();
        if preview_over(self.preview_until, now) {
            self.preview_until = None;
            self.show(self.state);
        }
        if preview_over(self.hold_until, now) {
            self.hold_until = None;
            if self.preview_until.is_none() {
                self.show(self.state);
                self.arm_cue();
            }
        }
        if preview_over(self.cue_until, now) {
            self.cue_until = None;
            if self.state.is_cue() {
                self.state = self.after_cue.take().unwrap_or(OverlayState::Hidden);
                if self.preview_until.is_none() && self.hold_until.is_none() {
                    self.show(self.state);
                }
            }
        }
    }

    /// Starts the end of a cue pill that is on screen now.
    fn arm_cue(&mut self) {
        self.cue_until = self.state.is_cue().then(|| Instant::now() + CUE);
        if self.cue_until.is_some() {
            self.wake_after(CUE);
        }
    }

    fn apply(&mut self, cmd: UiCmd, tray: &mut MainLoop) {
        match cmd {
            UiCmd::Overlay(state) => {
                if waits_for_cue(self.state, state) {
                    self.after_cue = Some(state);
                    return;
                }
                // A real state replaces a preview at once.
                self.preview_until = None;
                self.cue_until = None;
                self.after_cue = None;
                let held = self.hold_until.is_some_and(|t| Instant::now() < t);
                let left = hold(self.state, state, held, self.progress.finishing_left());
                self.state = state;
                if let Some(left) = left {
                    self.hold_until = Some(Instant::now() + left);
                    self.wake_after(left);
                } else {
                    self.hold_until = None;
                    self.show(state);
                    self.arm_cue();
                }
            }
            UiCmd::Tray(phase) => {
                // The recording icon wears the accent (DS-21.4); unchanged is a no-op.
                if let Err(e) = tray.set_accent(tray_accent(&self.cfg)) {
                    tracing::warn!("tray accent not updated: {e}");
                }
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
                self.wake_after(PREVIEW);
            }
            UiCmd::Config {
                overlay,
                meter_sensitivity_db,
                accent,
            } => {
                // A new accent shows on the tray at once, not at the next phase change.
                take_config(
                    &mut self.cfg,
                    overlay,
                    meter_sensitivity_db,
                    accent,
                    |accent| tray.set_accent(accent),
                );
                if self.preview_until.is_none() && self.hold_until.is_none() {
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

    /// Wakes the UI loop once `after` has passed (a preview or a held pill ends).
    fn wake_after(&self, after: Duration) {
        let waker = self.waker.clone();
        let timer = thread::Builder::new()
            .name("overlay-timer".into())
            .spawn(move || {
                thread::sleep(after);
                let _ = waker.wake();
            });
        if let Err(e) = timer {
            tracing::warn!(kind = ?e.kind(), "overlay timer not started");
        }
    }

    fn show(&mut self, state: OverlayState) {
        if let Err(e) = self.overlay.show(state, &self.cfg) {
            tracing::warn!("overlay not shown: {e}");
        }
    }
}

/// How long the pill of a transcription that just ended stays as it is, so its bar is seen to
/// run to the end: `finishing` is what the bar has left. A recording never waits. `held`: a
/// hold is already on, and whatever follows it waits too (the last state wins).
fn hold(
    was: OverlayState,
    next: OverlayState,
    held: bool,
    finishing: Option<Duration>,
) -> Option<Duration> {
    if matches!(next, OverlayState::Recording { .. }) {
        return None;
    }
    if held || was == OverlayState::Transcribing {
        return finishing.filter(|_| next != OverlayState::Transcribing);
    }
    None
}

/// Whether `next` waits until the cue `was` (on screen, or behind a held pill) has had its time:
/// the end of a dictation is seen even when the next one already runs (d03-label review #2). A
/// recording or another cue never waits.
fn waits_for_cue(was: OverlayState, next: OverlayState) -> bool {
    was.is_cue() && !next.is_cue() && !matches!(next, OverlayState::Recording { .. })
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

/// The seed of the accent setting, as the tray draws it.
fn tray_accent(cfg: &Config) -> Rgba {
    let Rgb(r, g, b) = seed(cfg.ui.accent);
    Rgba(r, g, b, 0xFF)
}

/// Puts the parts of a new active config the overlay and the tray read into `cfg`. The meter
/// sensitivity is read when a recording starts, so the next one meters with it; the accent at
/// the next pill drawn. `repaint_tray` gets the new accent at once.
fn take_config(
    cfg: &mut Config,
    overlay: config::Overlay,
    meter_sensitivity_db: f32,
    accent: config::Accent,
    repaint_tray: impl FnOnce(Rgba) -> std::io::Result<()>,
) {
    cfg.overlay = overlay;
    cfg.audio.meter_sensitivity_db = meter_sensitivity_db;
    cfg.ui.accent = accent;
    if let Err(e) = repaint_tray(tray_accent(cfg)) {
        tracing::warn!("tray accent not updated: {e}");
    }
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
    fn the_tray_draws_the_seed_of_the_accent_setting() {
        let mut cfg = Config::default();
        assert_eq!(
            tray_accent(&cfg),
            Rgba(0xD9, 0x3A, 0x3F, 0xFF),
            "Red by default"
        );
        cfg.ui.accent = config::Accent::Hex(Rgb(0x5B, 0x5B, 0xD6));
        assert_eq!(tray_accent(&cfg), Rgba(0x5B, 0x5B, 0xD6, 0xFF));
    }

    #[test]
    fn a_new_config_replaces_the_overlay_the_meter_sensitivity_and_the_accent() {
        let mut cfg = Config::default();
        let overlay = config::Overlay {
            enabled: !cfg.overlay.enabled,
            ..cfg.overlay.clone()
        };
        let iris = config::Accent::Hex(Rgb(0x5B, 0x5B, 0xD6));
        let mut painted = None;
        take_config(&mut cfg, overlay.clone(), -9.0, iris, |accent| {
            painted = Some(accent);
            Ok(())
        });
        assert_eq!(
            painted,
            Some(Rgba(0x5B, 0x5B, 0xD6, 0xFF)),
            "the tray is repainted at once"
        );
        assert_eq!(cfg.overlay, overlay);
        assert!((cfg.audio.meter_sensitivity_db + 9.0).abs() < f32::EPSILON);
        assert_eq!(cfg.ui.accent, iris);
        assert_eq!(
            tray_accent(&cfg),
            Rgba(0x5B, 0x5B, 0xD6, 0xFF),
            "the tray follows"
        );
    }

    #[test]
    fn the_next_step_waits_for_a_cue_and_a_recording_never_does() {
        let some = OverlayState::HeldBack { all: false };
        let saved = OverlayState::SavedToFile { also: true };
        let rec = OverlayState::Recording {
            locked: false,
            ending_soon: false,
        };
        assert!(waits_for_cue(some, OverlayState::Transcribing));
        assert!(waits_for_cue(saved, OverlayState::Hidden));
        assert!(!waits_for_cue(some, rec), "a press is shown at once");
        assert!(!waits_for_cue(some, saved), "a newer cue replaces it");
        assert!(!waits_for_cue(OverlayState::Pasting, OverlayState::Hidden));
    }

    #[test]
    fn a_finished_transcription_pill_waits_for_its_bar_and_a_new_recording_never_waits() {
        let left = Some(Duration::from_millis(300));
        let rec = OverlayState::Recording {
            locked: false,
            ending_soon: false,
        };
        let (t, p, h) = (
            OverlayState::Transcribing,
            OverlayState::Pasting,
            OverlayState::Hidden,
        );
        assert_eq!(hold(t, p, false, left), left);
        assert_eq!(hold(p, h, true, left), left, "the next state waits too");
        assert_eq!(hold(t, rec, false, left), None);
        assert_eq!(hold(t, p, false, None), None, "no bar to finish");
        assert_eq!(
            hold(p, h, false, left),
            None,
            "only a transcription pill waits"
        );
    }

    #[test]
    fn preview_ends_at_its_deadline() {
        let now = Instant::now();
        assert!(!preview_over(None, now));
        assert!(!preview_over(Some(now + PREVIEW), now));
        assert!(preview_over(Some(now), now));
    }
}
