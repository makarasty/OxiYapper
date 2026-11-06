//! Hotkey page: the chord field, the visual keyboard that picks a new chord, and the recording
//! mode. Keys are clicked on the board or pressed: while the board has the focus the daemon's
//! hook forwards them (`CaptureHotkey`); the page never hooks the keyboard itself.

#[path = "keyboard_layout.rs"]
mod keyboard_layout;

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak as RcWeak};
use std::time::Duration;

use oxi_core::hotkey::{Chord, ChordError, Key};
use oxi_core::ipc::{Push, Request};
use oxi_core::machine::Mode;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};

use crate::generated::{HotkeyState, KeyCap};
use crate::ipc::Event;
use crate::{AppWindow, Ctx, Page, report};

/// How often a running capture checks that its page is still shown.
// shortcut: the shell has no page-changed callback and a rail click leaves the focus on the
// board, so leaving the page is polled while capturing; use the callback once the shell has one.
const WATCH: Duration = Duration::from_millis(100);

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<HotkeyState>();
    let file = ctx.config.borrow();
    s.set_chord(chord_text(&file.config.hotkey.chord));
    s.set_mode(i32::from(file.config.hotkey.mode == Mode::Toggle));
    drop(file);
    s.set_columns(keyboard_layout::WIDTH);
    s.set_rows(keyboard_layout::HEIGHT);

    let layout = keyboard_layout::caps();
    let caps: Vec<KeyCap> = layout
        .iter()
        .map(|c| KeyCap {
            name: c.key.as_str().into(),
            label: c.key.label().into(),
            word: longest_word(c.key.label()),
            x: c.x,
            y: c.y,
            w: c.w,
            h: c.h,
            held: false,
            picked: false,
        })
        .collect();
    let picker = Rc::new_cyclic(|me| Picker {
        me: me.clone(),
        window: window.as_weak(),
        ctx: ctx.clone(),
        keys: layout.iter().map(|c| c.key).collect(),
        caps: Rc::new(VecModel::from(caps)),
        pending: RefCell::default(),
        held: RefCell::default(),
        capturing: Cell::new(false),
        watch: Timer::default(),
    });
    s.set_caps(ModelRc::from(picker.caps.clone()));
    picker.close();

    let p = picker.clone();
    s.on_shown(move || p.close());
    let p = picker.clone();
    s.on_change(move || p.open());
    let p = picker.clone();
    s.on_cancel(move || p.close());
    let p = picker.clone();
    s.on_apply(move || p.apply());
    let p = picker.clone();
    s.on_focused(move |on| p.capture(on));
    let p = picker.clone();
    s.on_key_clicked(move |name| {
        if let Some(key) = Key::from_name(&name) {
            p.clicked(key);
        }
    });
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_mode_picked(move |i| {
        let mode = if i == 1 { Mode::Toggle } else { Mode::Push };
        c.config.borrow_mut().config.hotkey.mode = mode;
        if let Some(window) = w.upgrade() {
            window.global::<HotkeyState>().set_mode(i);
        }
        report(&w, c.save());
    });

    // Reset to defaults binds again and subscribes once more: the old subscription only holds
    // the old picker weakly, so it goes quiet once the callbacks above replace it.
    let p = Rc::downgrade(&picker);
    ctx.ipc.subscribe(move |event| {
        if let (Some(p), Event::Push(Push::Key { code, down })) = (p.upgrade(), event) {
            p.pressed(*code, *down);
        }
    });
}

/// The open keyboard and the chord being picked on it.
struct Picker {
    me: RcWeak<Picker>,
    window: Weak<AppWindow>,
    ctx: Ctx,
    /// The key of each row of `caps`.
    keys: Vec<Key>,
    caps: Rc<VecModel<KeyCap>>,
    /// Keys of the pending chord, in the order picked.
    pending: RefCell<Vec<Key>>,
    /// Keys the daemon reports down.
    held: RefCell<Vec<Key>>,
    /// `CaptureHotkey { on: true }` was the last request sent.
    capturing: Cell<bool>,
    /// Runs while capturing: ends the capture once the page is left or the window hidden.
    watch: Timer,
}

impl Picker {
    fn open(&self) {
        // A disabled button still runs its accessible default action.
        if self.ctx.config.borrow().read_only {
            return;
        }
        self.pending.borrow_mut().clear();
        self.with(|s| {
            s.set_keyboard_open(true);
            s.set_error(SharedString::new());
        });
        self.capture(true);
    }

    fn close(&self) {
        self.capture(false);
        self.pending.borrow_mut().clear();
        self.with(|s| {
            s.set_keyboard_open(false);
            s.set_error(SharedString::new());
        });
        self.show();
    }

    /// Capture runs only while the keyboard is open and has the focus.
    fn capture(&self, on: bool) {
        let open = self
            .window
            .upgrade()
            .is_some_and(|w| w.global::<HotkeyState>().get_keyboard_open());
        let on = on && open;
        if self.capturing.replace(on) != on {
            self.ctx.ipc.send(Request::CaptureHotkey { on });
        }
        if on && !self.watch.running() {
            let me = self.me.clone();
            self.watch.start(TimerMode::Repeated, WATCH, move || {
                if let Some(p) = me.upgrade() {
                    p.check_page();
                }
            });
        }
        if !on {
            self.watch.stop();
            // No key-up arrives once capture is off.
            self.held.borrow_mut().clear();
        }
        self.show();
    }

    fn check_page(&self) {
        let shown = self
            .window
            .upgrade()
            .is_some_and(|w| w.get_page() == Page::Hotkey && w.window().is_visible());
        if !shown {
            self.close();
        }
    }

    fn clicked(&self, key: Key) {
        {
            let mut pending = self.pending.borrow_mut();
            if let Some(i) = pending.iter().position(|&k| k == key) {
                pending.remove(i);
            } else {
                pending.push(key);
            }
        }
        self.show();
    }

    /// A key from the hook. The first key down after all were up starts a new chord.
    fn pressed(&self, key: Key, down: bool) {
        if !self.capturing.get() {
            return;
        }
        {
            let mut held = self.held.borrow_mut();
            let mut pending = self.pending.borrow_mut();
            if down {
                if held.is_empty() {
                    pending.clear();
                }
                if !held.contains(&key) {
                    held.push(key);
                }
                if !pending.contains(&key) {
                    pending.push(key);
                }
            } else {
                held.retain(|&k| k != key);
            }
        }
        self.show();
    }

    fn apply(&self) {
        let chord = Chord::from_keys(&self.pending.borrow());
        match chord {
            Ok(chord) => {
                let names = chord.to_names();
                self.with(|s| s.set_chord(chord_text(&names)));
                self.ctx.config.borrow_mut().config.hotkey.chord = names;
                let saved = self.ctx.save();
                if saved.is_ok() {
                    self.close();
                }
                report(&self.window, saved);
            }
            Err(e) => self.with(|s| s.set_error(e.to_string().into())),
        }
    }

    /// Light the held and picked keys; show the pending chord or why it cannot be saved.
    fn show(&self) {
        let pending = self.pending.borrow();
        let held = self.held.borrow();
        for (i, key) in self.keys.iter().enumerate() {
            if let Some(mut cap) = self.caps.row_data(i) {
                let (h, p) = (held.contains(key), pending.contains(key));
                if (cap.held, cap.picked) != (h, p) {
                    (cap.held, cap.picked) = (h, p);
                    self.caps.set_row_data(i, cap);
                }
            }
        }
        let (text, error) = match Chord::from_keys(&pending) {
            Ok(chord) => (chord_text(&chord.to_names()), SharedString::new()),
            Err(ChordError::Empty) => (SharedString::new(), SharedString::new()),
            Err(e) => (SharedString::new(), e.to_string().into()),
        };
        self.with(|s| {
            s.set_pending(text);
            s.set_error(error);
        });
    }

    fn with(&self, f: impl FnOnce(HotkeyState<'_>)) {
        if let Some(window) = self.window.upgrade() {
            f(window.global::<HotkeyState>());
        }
    }
}

impl Drop for Picker {
    /// A window closed (or rebound by Reset) with the keyboard open ends the capture.
    fn drop(&mut self) {
        if self.capturing.get() {
            self.ctx.ipc.send(Request::CaptureHotkey { on: false });
        }
    }
}

/// Characters in the longest word of a keycap label.
fn longest_word(label: &str) -> i32 {
    let n = label.split(' ').map(|w| w.chars().count()).max();
    i32::try_from(n.unwrap_or(0)).unwrap_or(i32::MAX)
}

/// "Ctrl + Win".
fn chord_text(names: &[String]) -> SharedString {
    names.join(" + ").into()
}

#[cfg(test)]
mod tests {
    //! The page against the fake daemon link. `oxi_settings::open` makes its own offline link,
    //! so these build the window here with `Ipc::fake()`.
    // shortcut: move them to tests/page_hotkey.rs once lib.rs can open a window on a given Ipc.

    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::mpsc::Receiver;

    use i_slint_backend_testing::ElementHandle;
    use oxi_core::config::ConfigFile;
    use oxi_core::hotkey::Key;
    use oxi_core::ipc::{Push, Request};
    use oxi_core::paths::Layout;
    use slint::ComponentHandle;
    use slint::platform::WindowEvent;

    use crate::ipc::{Cmd, Event, Ipc};
    use crate::{AppWindow, Ctx, Page};

    fn open(name: &str) -> (AppWindow, Ipc, Receiver<Cmd>) {
        i_slint_backend_testing::init_no_event_loop();
        let dir = std::env::temp_dir().join(format!(
            "oxi-settings-unit-{}-hotkey-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (ipc, sent) = Ipc::fake();
        let ctx = Ctx {
            layout: Layout {
                config_dir: dir.clone(),
                data_dir: dir.clone(),
                logs_dir: dir,
            },
            config: Rc::new(RefCell::new(ConfigFile::default())),
            ipc: ipc.clone(),
        };
        let window = AppWindow::new().unwrap();
        crate::bind(&window, &ctx);
        window.show().unwrap();
        window.set_page(Page::Hotkey);
        (window, ipc, sent)
    }

    fn one(window: &AppWindow, label: &str) -> ElementHandle {
        ElementHandle::find_by_accessible_label(window, label)
            .next()
            .unwrap_or_else(|| panic!("no element labelled {label}"))
    }

    fn board_open(window: &AppWindow) -> bool {
        ElementHandle::find_by_accessible_label(window, "LCtrl")
            .next()
            .is_some()
    }

    fn captures(sent: &Receiver<Cmd>) -> Vec<bool> {
        sent.try_iter()
            .filter_map(|c| match c {
                Cmd::Send(Request::CaptureHotkey { on }) => Some(on),
                _ => None,
            })
            .collect()
    }

    fn key(ipc: &Ipc, code: Key, down: bool) {
        ipc.deliver(&Event::Push(Push::Key { code, down }));
    }

    #[test]
    fn opening_captures_and_closing_ends_it() {
        let (window, _ipc, sent) = open("close");
        assert_eq!(captures(&sent), Vec::<bool>::new());
        one(&window, "Change hotkey").invoke_accessible_default_action();
        // The query builds the board, which takes the focus.
        assert!(board_open(&window));
        assert_eq!(captures(&sent), [true]);
        one(&window, "Cancel hotkey change").invoke_accessible_default_action();
        assert_eq!(captures(&sent), [false]);
        assert!(!board_open(&window));
    }

    #[test]
    fn pushed_keys_light_up_and_fill_the_chord() {
        let (window, ipc, _sent) = open("push");
        one(&window, "Change hotkey").invoke_accessible_default_action();
        key(&ipc, Key::LShift, true);
        let shift = one(&window, "LShift");
        assert_eq!(shift.accessible_description().as_deref(), Some("held"));
        assert_eq!(shift.accessible_checked(), Some(true));
        key(&ipc, Key::F13, true);
        assert!(one(&window, "New hotkey: Shift + F13").is_valid());
        key(&ipc, Key::LShift, false);
        key(&ipc, Key::F13, false);
        assert_eq!(
            one(&window, "LShift").accessible_description().as_deref(),
            Some("")
        );
        // A new press after all keys are up starts a new chord.
        key(&ipc, Key::F14, true);
        assert!(one(&window, "New hotkey: F14").is_valid());
        assert_eq!(one(&window, "LShift").accessible_checked(), Some(false));
    }

    #[test]
    fn focus_loss_ends_capture_and_drops_late_keys() {
        let (window, ipc, sent) = open("focus");
        one(&window, "Change hotkey").invoke_accessible_default_action();
        let _ = one(&window, "LCtrl");
        assert_eq!(captures(&sent), [true]);
        window
            .window()
            .dispatch_event(WindowEvent::WindowActiveChanged(false));
        assert_eq!(captures(&sent), [false]);
        key(&ipc, Key::LAlt, true);
        assert_eq!(one(&window, "LAlt").accessible_checked(), Some(false));
        // Back in front: the board still has the focus, capture resumes.
        window
            .window()
            .dispatch_event(WindowEvent::WindowActiveChanged(true));
        assert_eq!(captures(&sent), [true]);
    }

    #[test]
    fn leaving_the_page_from_the_rail_ends_capture() {
        let (window, _ipc, sent) = open("leave");
        one(&window, "Change hotkey").invoke_accessible_default_action();
        let _ = one(&window, "LCtrl");
        assert_eq!(captures(&sent), [true]);
        one(&window, "Audio").mock_single_click(slint::platform::PointerEventButton::Left);
        assert_eq!(window.get_page(), Page::Audio);
        i_slint_backend_testing::mock_elapsed_time(super::WATCH * 2);
        assert_eq!(captures(&sent), [false]);
        // Back on the page, the board is closed.
        window.set_page(Page::Hotkey);
        assert!(!board_open(&window));
    }

    #[test]
    fn closing_the_window_ends_capture() {
        let (window, _ipc, sent) = open("drop");
        one(&window, "Change hotkey").invoke_accessible_default_action();
        let _ = one(&window, "LCtrl");
        assert_eq!(captures(&sent), [true]);
        window.hide().unwrap();
        drop(window);
        assert_eq!(captures(&sent), [false]);
    }
}
