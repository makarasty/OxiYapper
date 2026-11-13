//! The real settings window for `scripts/real-shot.ps1`: the same renderer selection as
//! `OxiYapper settings`, a throwaway data dir, a fake daemon link, placed off every screen.
//!
//! `real_window <page> <width> <height> <light|dark|system> <out.png> [tab]`: once the window has
//! settled it saves the renderer's own snapshot of the client area to `out.png`, prints
//! `real_window: shot <w>x<h>` and quits; after `DEADLINE` it quits on its own if that never happened. The snapshot
//! comes from the runtime renderer, not from the OS: `PrintWindow` of a window kept off every screen
//! came back flat white in about 4 runs of 10 (the frame is only there if it is read early).
//! The window is never activated, so it cannot take the owner's focus. The Audio page still opens
//! the default microphone for its level meter (`pages/audio.rs`, in memory, nothing is written).
//! `--idle <seconds>` (anywhere in the arguments, `out.png` is then unused) takes no shot: the window
//! stays open that long, untouched, for the idle-CPU measurement of UI rule 23, then exits.
//! `tab` opens a sub-tab by name; a name it does not know is an error, never a shot of another tab.
//!
//! States a plain start cannot reach come from the environment (all synthetic, all optional):
//! - `OXI_SHOT_BANNER=<text>`: the broken-config banner with that text.
//! - `OXI_SHOT_RULES=<n>`: `n` per-app rules in the config.
//! - `OXI_SHOT_HISTORY=<n>`: `n` History records, every seventh a long one.
//! - `OXI_SHOT_STATS=1`: about half a year of Home statistics.
//! - `OXI_SHOT_WORDS=ready|pending`: the newest record gets a short tone as its audio; `ready`
//!   answers the request for word times (one doubted word), `pending` leaves it unanswered.
//! - `OXI_SHOT_CLICK=x,y;hx,y;...`: before the shot, a left click at each `x,y` (logical px) or,
//!   with `h`, only the pointer moved there, one step per settle. Never aim one at Play or Listen
//!   (the page plays through the real speakers), at a folder or file button, or at the daemon
//!   banner's Start.

// No console: a console exe started by the script shares the caller's console and was killed
// from outside in about 1 run of 40 (exit 0xC000013A, or 0xC0000142 while attaching to it). The
// script's stderr redirect still works without one.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::{env, fs, path::PathBuf, process, time::Duration};

use oxi_core::history::{HistoryId, Status, Word};
use oxi_core::ipc::{HistoryItem, Push, Request, Spoken, Words, WordsResult};
use oxi_core::paths::Layout;
use oxi_core::stats::{HEAT_WEEKS, Stats, Summary};
use oxi_core::types::Millis;
use oxi_settings::ipc::{Cmd, Event};
use oxi_settings::{
    AppWindow, AudioState, Ipc, ModelState, Page, SystemState, TestState, TextState, ThemeChoice,
};
use slint::platform::{PointerEventButton, WindowEvent};
use slint::{ComponentHandle, LogicalPosition, LogicalSize, PhysicalPosition, Timer, TimerMode};

/// Far outside any monitor, but inside the 16-bit range window coordinates keep.
const OFF_SCREEN: i32 = -20_000;

/// Longest this process lives; the script waits for far less.
const DEADLINE: Duration = Duration::from_secs(60);

/// First snapshot attempt, and the gap between attempts: the window is created at a default size
/// and the layout pass and animations settle a moment after the first frame.
const SETTLE: Duration = Duration::from_millis(700);
const RETRY: Duration = Duration::from_millis(200);

/// Win32: a window this thread is about to show gets `WS_EX_NOACTIVATE`. winit shows a new window
/// with `SW_SHOW`, which takes the foreground once the owner has been idle past the foreground
/// lock timeout; Slint has no switch for it. The style is set on `WM_SHOWWINDOW` because winit
/// rewrites the whole extended style before that, and cancelling the activation instead
/// (`HCBT_ACTIVATE`) leaves the owner with no foreground window at all.
#[cfg(windows)]
mod no_activate {
    use std::ffi::c_void;

    const WH_CALLWNDPROC: i32 = 4;
    const WM_SHOWWINDOW: u32 = 0x0018;
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_NOACTIVATE: i32 = 0x0800_0000;

    /// `CWPSTRUCT`.
    #[repr(C)]
    struct CallWndProc {
        l: isize,
        w: usize,
        message: u32,
        hwnd: *mut c_void,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowsHookExW(
            id: i32,
            proc: unsafe extern "system" fn(i32, usize, isize) -> isize,
            module: *mut c_void,
            thread: u32,
        ) -> *mut c_void;
        fn CallNextHookEx(hook: *mut c_void, code: i32, w: usize, l: isize) -> isize;
        fn GetWindowLongW(hwnd: *mut c_void, index: i32) -> i32;
        fn SetWindowLongW(hwnd: *mut c_void, index: i32, value: i32) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }

    unsafe extern "system" fn on_message(code: i32, w: usize, l: isize) -> isize {
        // SAFETY: for `HC_ACTION` (0) `l` points at the system's live `CWPSTRUCT`; the style calls
        // take the window the message is for.
        unsafe {
            let msg = &*(l as *const CallWndProc);
            if code == 0 && msg.message == WM_SHOWWINDOW && msg.w != 0 {
                let ex = GetWindowLongW(msg.hwnd, GWL_EXSTYLE);
                SetWindowLongW(msg.hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE);
            }
            CallNextHookEx(std::ptr::null_mut(), code, w, l)
        }
    }

    /// Installs the hook for the calling thread, which must be the one that shows the window.
    pub fn install() -> Result<(), &'static str> {
        // SAFETY: `on_message` matches the HOOKPROC signature and lives for the whole process; a
        // thread hook needs no module handle.
        let hook = unsafe {
            SetWindowsHookExW(
                WH_CALLWNDPROC,
                on_message,
                std::ptr::null_mut(),
                GetCurrentThreadId(),
            )
        };
        if hook.is_null() {
            Err("cannot install the no-activate hook")
        } else {
            Ok(())
        }
    }
}

fn page(name: &str) -> Option<Page> {
    Some(match name {
        "home" => Page::Home,
        "hotkey" => Page::Hotkey,
        "audio" => Page::Audio,
        "model" => Page::Model,
        "text" => Page::Text,
        "overlay" => Page::Overlay,
        "history" => Page::History,
        "test" => Page::Test,
        "system" => Page::System,
        "about" => Page::About,
        _ => return None,
    })
}

fn open_tab(window: &AppWindow, page: Page, name: &str) -> Result<(), String> {
    let tabs: &[&str] = match page {
        Page::Test => &["record", "results"],
        Page::Audio => &["microphone", "recording", "start", "stop"],
        Page::Model => &["models", "settings", "advanced"],
        Page::Text => &[
            "dictionary",
            "replacements",
            "paste",
            "rules",
            "processing",
            "words",
        ],
        Page::System => &["general", "recordings"],
        _ => &[],
    };
    let Some(view) = tabs.iter().position(|t| *t == name) else {
        return Err(format!("no sub-tab \"{name}\" on this page: {tabs:?}"));
    };
    let view = i32::try_from(view).map_err(|e| e.to_string())?;
    match page {
        Page::Test => window.global::<TestState>().set_view(view),
        Page::Audio => window.global::<AudioState>().set_view(view),
        Page::Model => window.global::<ModelState>().set_tab(view),
        Page::Text => window.global::<TextState>().set_tab(view),
        Page::System => window.global::<SystemState>().set_tab(view),
        _ => {}
    }
    Ok(())
}

/// `OXI_SHOT_CLICK`: the pointer steps before the shot, `(hover only, x, y)`.
fn pointer_steps() -> Result<VecDeque<(bool, f32, f32)>, String> {
    let Ok(list) = env::var("OXI_SHOT_CLICK") else {
        return Ok(VecDeque::new());
    };
    list.split(';')
        .map(|step| {
            let (hover, at) = step
                .strip_prefix('h')
                .map_or((false, step), |at| (true, at));
            at.split_once(',')
                .and_then(|(x, y)| Some((hover, x.trim().parse().ok()?, y.trim().parse().ok()?)))
                .ok_or_else(|| format!("OXI_SHOT_CLICK: bad step \"{step}\""))
        })
        .collect()
}

/// `OXI_SHOT_RULES`, `OXI_SHOT_STATS` and `OXI_SHOT_WORDS`: the synthetic files the pages read,
/// written before the window opens. Returns the summary the fake daemon answers `Stats` with.
fn seed(layout: &Layout) -> Result<Option<Summary>, Box<dyn std::error::Error>> {
    let rules = env::var("OXI_SHOT_RULES").ok();
    if let Some(n) = rules.and_then(|n| n.parse::<u32>().ok()) {
        let mut text = String::from("version = 2\n");
        for i in 0..n {
            let _ = write!(
                text,
                "[[paste.rules]]\napp = \"synthapp{i}.exe\"\nmode = \"type\"\n"
            );
        }
        fs::write(layout.config_file(), text)?;
    }
    if env::var_os("OXI_SHOT_WORDS").is_some() {
        let day = layout.history_dir().join("2026-11-10");
        fs::create_dir_all(&day)?;
        // 4.5 s (the record's length) of a 160 Hz saw whose loudness steps, so the wave has a shape.
        let part = |n: u32| f32::from(u16::try_from(n).unwrap_or(0));
        let samples: Vec<f32> = (0..72_000_u32)
            .map(|i| (part(i % 100) / 100.0 - 0.5) * (part(i / 3_600 % 5) + 1.0) / 6.0)
            .collect();
        oxi_platform::wav::write_wav(&day.join("120000-000.wav"), &samples)
            .map_err(|_| "cannot write the synthetic recording")?;
    }
    if env::var_os("OXI_SHOT_STATS").is_none() {
        return Ok(None);
    }
    let today = oxi_platform::stats_store::today();
    let mut stats = Stats::default();
    for back in (0..170_u32).filter(|back| back % 6 != 4) {
        let words = u64::from(back * 37 % 400) + 20;
        let day = today.add_days(-i32::try_from(back).unwrap_or(0));
        stats.record_voiced(day, words, Millis(words * 500), Millis(words * 400));
    }
    let json = stats
        .to_json()
        .map_err(|_| "cannot write the synthetic stats")?;
    fs::write(layout.data_dir.join(oxi_core::stats::FILE), json)?;
    Ok(Some(stats.summary(today, HEAT_WEEKS)))
}

/// The shot's config folder: removed when dropped, so every way out of `main` cleans up.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[allow(clippy::too_many_lines)] // one linear setup script; splitting it hides the order
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let mut idle = None;
    if let Some(i) = args.iter().position(|a| a == "--idle") {
        idle = args.get(i + 1).and_then(|n| n.parse::<u64>().ok());
        if idle.is_none() {
            return Err("--idle needs a whole number of seconds".into());
        }
        args.drain(i..=i + 1);
    }
    let mut args = args.into_iter();
    let usage = "usage: real_window <page> <width> <height> <light|dark|system> <out.png> [tab]";
    let (Some(name), Some(w), Some(h), Some(theme), Some(out)) = (
        args.next(),
        args.next(),
        args.next(),
        args.next(),
        args.next(),
    ) else {
        return Err(usage.into());
    };
    let (Some(page), Ok(w), Ok(h)) = (page(&name), w.parse::<f32>(), h.parse::<f32>()) else {
        return Err(usage.into());
    };
    let theme = match theme.as_str() {
        "light" => ThemeChoice::Light,
        "dark" => ThemeChoice::Dark,
        // Follows the OS scheme of this machine, as Appearance = System does.
        "system" => ThemeChoice::System,
        _ => return Err(usage.into()),
    };
    let out = PathBuf::from(out);
    let tab = args.next();

    #[cfg(windows)]
    no_activate::install()?;

    let dir = env::temp_dir().join(format!("oxi-real-shot-{}", process::id()));
    fs::create_dir_all(&dir)?;
    let _temp = TempDir(dir.clone());
    let layout = Layout {
        config_dir: dir.clone(),
        data_dir: dir.clone(),
        logs_dir: dir.clone(),
    };

    let mut steps = pointer_steps()?;
    let summary = seed(&layout)?;
    let records = env::var("OXI_SHOT_HISTORY")
        .ok()
        .and_then(|n| n.parse::<u32>().ok());
    let words_ready = env::var("OXI_SHOT_WORDS").is_ok_and(|w| w == "ready");

    let skia = oxi_settings::use_skia();
    let (ipc, daemon_side) = Ipc::fake();
    let window = oxi_settings::open_with(layout, ipc.clone())?;
    window.set_page(page);
    if let Ok(text) = env::var("OXI_SHOT_BANNER") {
        window.set_banner_text(text.into());
        window.set_banner_fixable(true);
    }
    if let Some(tab) = &tab
        && let Err(e) = open_tab(&window, page, tab)
    {
        return Err(e.into());
    }
    window.set_theme(theme);
    window.window().set_size(LogicalSize::new(w, h));
    window
        .window()
        .set_position(PhysicalPosition::new(OFF_SCREEN, OFF_SCREEN));
    eprintln!(
        "real_window: renderer {}",
        if skia { "skia-software" } else { "software" }
    );
    // Like `run`: the backend writes the system accent when it shows the window, so put ours back
    // before the first frame can be captured.
    let weak = window.as_weak();
    slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            oxi_settings::own_accent(&window);
            oxi_settings::caption_theme(&window, window.get_theme());
        }
    })?;
    Timer::single_shot(idle.map_or(DEADLINE, Duration::from_secs), || {
        let _ = slint::quit_event_loop();
    });
    // The daemon's side: the History page asks for its list and waits for the answer.
    let daemon = Timer::default();
    daemon.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        while let Ok(cmd) = daemon_side.try_recv() {
            match cmd {
                Cmd::Send(Request::HistoryList { .. }) => {
                    ipc.deliver(&Event::Push(Push::History {
                        items: sample_history(records),
                        more: false,
                    }));
                }
                Cmd::Send(Request::Stats) => {
                    if let Some(summary) = &summary {
                        ipc.deliver(&Event::Push(Push::Stats(summary.clone())));
                    }
                }
                Cmd::Send(Request::HistoryWords { run, id }) if words_ready => {
                    ipc.deliver(&Event::Push(Push::HistoryWords {
                        run,
                        id,
                        result: sample_words(),
                    }));
                }
                _ => {}
            }
        }
    });
    let shooter = Timer::default();
    let weak = window.as_weak();
    let want = (w, h);
    // Idle mode starts no timer of its own: it must not add load to what it measures.
    if idle.is_none() {
        shooter.start(TimerMode::Repeated, RETRY, {
            let mut started = std::time::Instant::now();
            move || {
                if started.elapsed() < SETTLE {
                    return;
                }
                let Some(window) = weak.upgrade() else { return };
                let Ok(shot) = window.window().take_snapshot() else {
                    return;
                };
                let scale = window.window().scale_factor();
                // The window is still at its default size until the resize lands.
                let off =
                    |got: u32, want: f32| (f64::from(got) - f64::from(want * scale)).abs() > 1.0;
                if off(shot.width(), want.0) || off(shot.height(), want.1) {
                    return;
                }
                // One pointer step, then the window settles again before the next or the shot.
                if let Some((hover, x, y)) = steps.pop_front() {
                    let position = LogicalPosition::new(x, y);
                    let button = PointerEventButton::Left;
                    let w = window.window();
                    w.dispatch_event(WindowEvent::PointerMoved { position });
                    if !hover {
                        w.dispatch_event(WindowEvent::PointerPressed { position, button });
                        w.dispatch_event(WindowEvent::PointerReleased { position, button });
                    }
                    started = std::time::Instant::now();
                    return;
                }
                let size = tiny_skia::IntSize::from_wh(shot.width(), shot.height());
                let Some(pixmap) = size
                    .and_then(|size| tiny_skia::Pixmap::from_vec(shot.as_bytes().to_vec(), size))
                else {
                    return;
                };
                if let Err(e) = pixmap.save_png(&out) {
                    eprintln!("real_window: cannot save {}: {e}", out.display());
                    // `exit` runs no destructors.
                    let _ = fs::remove_dir_all(&dir);
                    process::exit(2);
                }
                eprintln!("real_window: shot {}x{}", shot.width(), shot.height());
                let _ = slint::quit_event_loop();
            }
        });
    }
    let result = window.run();
    Ok(result?)
}

/// The word times of "Sample dictation one", its second word one the model doubted.
fn sample_words() -> WordsResult {
    let word = |w: &str, p, t0, t1| Word {
        w: w.into(),
        p,
        t0: Some(t0),
        t1: Some(t1),
    };
    WordsResult::Ready {
        words: Words(vec![
            word("Sample", 0.95, 200, 900),
            word("dictation", 0.3, 1_100, 2_300),
            word("one", 0.9, 2_600, 3_400),
        ]),
        timed: true,
    }
}

/// Synthetic dictations for the History page (no real person's text): four, or `records` of them.
fn sample_history(records: Option<u32>) -> Vec<HistoryItem> {
    let item = |id: &str, status: Status, text: &str| HistoryItem {
        id: HistoryId::parse(id).unwrap(),
        status,
        duration_ms: 4_500,
        model: String::new(),
        language: String::new(),
        text: Spoken(text.into()),
        held: Vec::new(),
    };
    // The newest has a part held back, so the "Not inserted" row can be shot.
    let mut first = item(
        "2026-11-10/120000-000",
        Status::Done,
        "Sample dictation one",
    );
    first.held = vec![oxi_core::ipc::HeldText {
        text: Spoken("sample held words".into()),
        why: oxi_core::text::HeldWhy::Hallucination,
    }];
    let mut items = vec![
        first,
        item(
            "2026-11-10/115000-000",
            Status::Done,
            "Sample dictation two",
        ),
        item("2026-11-09/090000-000", Status::Cancelled, ""),
        item(
            "2026-11-09/085500-000",
            Status::Done,
            "Sample dictation three",
        ),
    ];
    // Older days, newest first: 300 a day, 200 s apart.
    for i in 0..records.unwrap_or(0).saturating_sub(4) {
        let (day, s) = (8 - (i / 300).min(7), 86_000 - i % 300 * 200);
        let id = format!(
            "2026-11-{day:02}/{:02}{:02}{:02}-000",
            s / 3_600,
            s / 60 % 60,
            s % 60
        );
        let text = if i % 7 == 0 {
            format!("Sample long dictation {i} ").repeat(40)
        } else {
            format!("Sample dictation {i}")
        };
        items.push(item(&id, Status::Done, &text));
    }
    items
}
