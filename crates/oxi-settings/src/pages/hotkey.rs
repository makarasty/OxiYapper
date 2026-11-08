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
use crate::ipc::{Event, Link, Subscription};
use crate::{AppWindow, Ctx, Page, report};

/// How often a running capture checks that its page is still shown.
// shortcut: the shell has no page-changed callback and a rail click leaves the focus on the
// board, so leaving the page is polled while capturing; use the callback once the shell has one.
const WATCH: Duration = Duration::from_millis(100);

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<HotkeyState>();
    let file = ctx.config.borrow();
    set_saved_chord(&s, &file.config.hotkey.chord);
    s.set_mode(i32::from(file.config.hotkey.mode == Mode::Toggle));
    drop(file);
    s.set_columns(keyboard_layout::WIDTH);
    s.set_rows(keyboard_layout::HEIGHT);
    s.set_online(ctx.ipc.link() == Link::Online);

    let layout = keyboard_layout::caps();
    let caps: Vec<KeyCap> = layout
        .iter()
        .map(|c| KeyCap {
            name: c.key.as_str().into(),
            label: keyboard_layout::cap_label(c.key, true).into(),
            short: keyboard_layout::cap_label(c.key, false).into(),
            full_from: keyboard_layout::full_from(c.key),
            spoken: c.key.spoken().into(),
            x: c.x,
            y: c.y,
            w: c.w,
            h: c.h,
            held: false,
            picked: false,
            invalid: false,
        })
        .collect();
    let picker = Rc::new_cyclic(|me: &RcWeak<Picker>| Picker {
        me: me.clone(),
        // Reset to defaults binds again: the callbacks below replace the old picker's, which
        // drops it and with it this subscription.
        _events: ctx.ipc.subscribe({
            let me = me.clone();
            move |event| {
                let Some(p) = me.upgrade() else { return };
                match event {
                    Event::Push(Push::Key { code, down }) => p.pressed(*code, *down),
                    Event::Link(link) => p.linked(*link),
                    Event::Push(_) => {}
                }
            }
        }),
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
}

/// The open keyboard and the chord being picked on it.
struct Picker {
    me: RcWeak<Picker>,
    _events: Subscription,
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
        if on {
            self.with(|s| s.set_move_focus(0));
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

    /// Without a daemon nothing captures and Apply is off. A new one (restart) has no capture:
    /// ask again. Keys whose key-up was lost are forgotten either way.
    fn linked(&self, link: Link) {
        let online = link == Link::Online;
        self.with(|s| s.set_online(online));
        if self.capturing.get() {
            self.held.borrow_mut().clear();
            if online {
                self.ctx.ipc.send(Request::CaptureHotkey { on: true });
            }
            self.show();
        }
    }

    /// Ends the capture from the keyboard (the hook swallows Tab and Esc, so the board would
    /// trap the focus) and moves the focus to Apply (1) or Cancel (2).
    fn hand_back(&self, to: i32) {
        self.capture(false);
        self.with(|s| s.set_move_focus(to));
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

    /// A key from the hook. The first key down after all were up starts a new chord. Esc alone
    /// goes to Cancel; releasing every key of a valid chord goes to Apply.
    fn pressed(&self, key: Key, down: bool) {
        if !self.capturing.get() {
            return;
        }
        if down && key == Key::Esc && self.held.borrow().is_empty() {
            self.hand_back(2);
            return;
        }
        let released = {
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
            !down && held.is_empty() && Chord::from_keys(&pending).is_ok()
        };
        if released {
            self.hand_back(1);
        } else {
            self.show();
        }
    }

    fn apply(&self) {
        // A disabled button still runs its accessible default action.
        if self.ctx.ipc.link() != Link::Online {
            return;
        }
        let chord = Chord::from_keys(&self.pending.borrow());
        match chord {
            Ok(chord) => {
                let names = chord.to_names();
                self.with(|s| set_saved_chord(&s, &names));
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
                // Esc is the one key a chord refuses (`ChordError::Esc`).
                let bad = p && *key == Key::Esc;
                if (cap.held, cap.picked, cap.invalid) != (h, p, bad) {
                    (cap.held, cap.picked, cap.invalid) = (h, p, bad);
                    self.caps.set_row_data(i, cap);
                }
            }
        }
        let chord = Chord::from_keys(&pending);
        // A refused chord still shows what was pressed; a valid one shows as it would be saved.
        let keys = match &chord {
            Ok(chord) => chord.to_names(),
            Err(_) => pending.iter().map(|k| k.label().to_owned()).collect(),
        };
        let (text, error) = match chord {
            Ok(chord) => (chord_text(&chord.to_names()), SharedString::new()),
            Err(ChordError::Empty) => (SharedString::new(), SharedString::new()),
            Err(e) => (SharedString::new(), e.to_string().into()),
        };
        self.with(|s| {
            s.set_pending(text);
            s.set_pending_keys(string_model(&keys));
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

/// "Ctrl + Win".
fn chord_text(names: &[String]) -> SharedString {
    names.join(" + ").into()
}

fn string_model(items: &[String]) -> ModelRc<SharedString> {
    let items: Vec<SharedString> = items.iter().map(SharedString::from).collect();
    ModelRc::from(Rc::new(VecModel::from(items)))
}

/// Show `names` as the saved chord: the text for screen readers and the keycaps.
fn set_saved_chord(s: &HotkeyState<'_>, names: &[String]) {
    s.set_chord(chord_text(names));
    s.set_chord_keys(string_model(names));
}

#[cfg(test)]
mod tests {
    //! The page against the fake daemon link.

    use std::sync::mpsc::Receiver;

    use i_slint_backend_testing::ElementHandle;
    use oxi_core::hotkey::Key;
    use oxi_core::ipc::{Push, Request};
    use slint::ComponentHandle;
    use slint::platform::WindowEvent;

    use crate::generated::HotkeyState;
    use crate::ipc::{Cmd, Event, Ipc, Link};
    use crate::testing::{linked, one};
    use crate::{AppWindow, Page};

    fn open(name: &str) -> (AppWindow, Ipc, Receiver<Cmd>) {
        let (window, ipc, sent, _) = linked("hotkey", name, Page::Hotkey);
        (window, ipc, sent)
    }

    fn board_open(window: &AppWindow) -> bool {
        ElementHandle::find_by_accessible_label(window, "Left Ctrl")
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
    fn apply_is_off_while_the_chord_is_invalid() {
        let (window, ipc, _sent) = open("invalid-apply");
        one(&window, "Change hotkey").invoke_accessible_default_action();
        key(&ipc, Key::F13, true);
        assert_eq!(
            one(&window, "Apply hotkey").accessible_enabled(),
            Some(true)
        );
        // Esc inside a chord shows the error: nothing to apply until a valid chord is pressed.
        key(&ipc, Key::Esc, true);
        assert!(!window.global::<HotkeyState>().get_error().is_empty());
        assert_eq!(
            one(&window, "Apply hotkey").accessible_enabled(),
            Some(false)
        );
        for k in [Key::F13, Key::Esc] {
            key(&ipc, k, false);
        }
        key(&ipc, Key::F14, true);
        assert_eq!(
            one(&window, "Apply hotkey").accessible_enabled(),
            Some(true)
        );
    }

    #[test]
    fn pushed_keys_light_up_and_fill_the_chord() {
        let (window, ipc, _sent) = open("push");
        one(&window, "Change hotkey").invoke_accessible_default_action();
        key(&ipc, Key::LShift, true);
        let shift = one(&window, "Left Shift");
        assert_eq!(shift.accessible_description().as_deref(), Some("held"));
        assert_eq!(shift.accessible_checked(), Some(true));
        key(&ipc, Key::F13, true);
        assert!(one(&window, "New hotkey: Shift + F13").is_valid());
        // Esc inside a chord is refused, and capture goes on once every key is up.
        key(&ipc, Key::Esc, true);
        for k in [Key::LShift, Key::F13, Key::Esc] {
            key(&ipc, k, false);
        }
        assert_eq!(
            one(&window, "Left Shift")
                .accessible_description()
                .as_deref(),
            Some("")
        );
        // A new press after all keys are up starts a new chord.
        key(&ipc, Key::F14, true);
        assert!(one(&window, "New hotkey: F14").is_valid());
        assert_eq!(one(&window, "Left Shift").accessible_checked(), Some(false));
    }

    fn press(window: &AppWindow, k: slint::platform::Key) {
        // The focus moves in a change handler, which runs with the timers.
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
        let text: slint::SharedString = k.into();
        let w = window.window();
        w.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
        w.dispatch_event(WindowEvent::KeyReleased { text });
    }

    #[test]
    fn releasing_a_chord_ends_capture_and_space_applies_it() {
        let (window, ipc, sent) = open("release");
        one(&window, "Change hotkey").invoke_accessible_default_action();
        let _ = one(&window, "Left Ctrl");
        for (k, down) in [(Key::LShift, true), (Key::F13, true), (Key::F13, false)] {
            key(&ipc, k, down);
        }
        assert_eq!(captures(&sent), [true]);
        key(&ipc, Key::LShift, false);
        assert_eq!(captures(&sent), [false]);
        // The hook is off, so the window gets the keys: the focus is on Apply.
        press(&window, slint::platform::Key::Space);
        assert!(!board_open(&window));
        assert!(one(&window, "Shift + F13").is_valid());
        assert_eq!(captures(&sent), Vec::<bool>::new());
    }

    #[test]
    fn esc_alone_ends_capture_and_space_cancels() {
        let (window, ipc, sent) = open("escape");
        one(&window, "Change hotkey").invoke_accessible_default_action();
        let _ = one(&window, "Left Ctrl");
        key(&ipc, Key::F13, true);
        key(&ipc, Key::F13, false);
        assert_eq!(captures(&sent), [true, false]);
        // Back on the board, capture runs again; Esc alone hands the focus to Cancel.
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
        one(&window, "F5").mock_single_click(slint::platform::PointerEventButton::Left);
        assert_eq!(captures(&sent), [true]);
        key(&ipc, Key::Esc, true);
        assert_eq!(captures(&sent), [false]);
        press(&window, slint::platform::Key::Space);
        assert!(!board_open(&window));
        assert!(one(&window, "Ctrl + Win").is_valid());
    }

    #[test]
    fn a_restarted_daemon_is_asked_to_capture_again() {
        let (window, ipc, sent) = open("relink");
        one(&window, "Change hotkey").invoke_accessible_default_action();
        let _ = one(&window, "Left Ctrl");
        key(&ipc, Key::LAlt, true);
        assert_eq!(captures(&sent), [true]);
        ipc.deliver(&Event::Link(Link::Offline));
        ipc.deliver(&Event::Link(Link::Online));
        assert_eq!(captures(&sent), [true]);
        // The lost key-up of Alt does not stick.
        assert_eq!(
            one(&window, "Left Alt").accessible_description().as_deref(),
            Some("")
        );
        one(&window, "Cancel hotkey change").invoke_accessible_default_action();
        assert_eq!(captures(&sent), [false]);
        ipc.deliver(&Event::Link(Link::Online));
        assert_eq!(captures(&sent), Vec::<bool>::new());
    }

    #[test]
    fn the_board_grows_with_the_window_and_stops_at_52() {
        let (window, _ipc, _sent) = open("scale");
        one(&window, "Change hotkey").invoke_accessible_default_action();
        let cap = |w: f32| {
            window.window().set_size(slint::LogicalSize::new(w, 900.0));
            one(&window, "F1").size().width
        };
        let small = cap(720.0);
        let middle = cap(1120.0);
        let large = cap(1600.0);
        let huge = cap(2400.0);
        // A cap is the unit less its gap: 28 px units give 25 px caps, 36 give 33, 52 give 47.8.
        assert!((24.0..=25.5).contains(&small), "720: {small}");
        assert!((32.0..36.0).contains(&middle), "1120: {middle}");
        assert!((large - 52.0 * 0.92).abs() < 0.5, "1600: {large}");
        assert!((huge - large).abs() < 0.01, "stopped growing: {huge}");
    }

    #[test]
    fn cap_labels_stay_readable_at_every_unit() {
        let (window, _ipc, _sent) = open("labels");
        let font = window.global::<crate::generated::CapFont>();
        assert!((font.invoke_size(26.0) - 12.0).abs() < 0.01, "floor");
        assert!((font.invoke_size(40.0) - 12.0).abs() < 0.01);
        assert!((font.invoke_size(52.0) - 15.0).abs() < 0.01, "ceiling");
    }

    #[test]
    fn esc_in_a_chord_is_marked_invalid_on_its_cap() {
        let (window, ipc, _sent) = open("invalid-cap");
        one(&window, "Change hotkey").invoke_accessible_default_action();
        key(&ipc, Key::LCtrl, true);
        key(&ipc, Key::Esc, true);
        let caps = window.global::<HotkeyState>().get_caps();
        let esc = slint::Model::iter(&caps)
            .find(|c| c.name == Key::Esc.as_str())
            .unwrap();
        assert!(esc.invalid);
        let ctrl = slint::Model::iter(&caps)
            .find(|c| c.name == Key::LCtrl.as_str())
            .unwrap();
        assert!(!ctrl.invalid);
    }

    #[test]
    fn a_lost_daemon_clears_held_keys_and_turns_apply_off_until_it_is_back() {
        let (window, ipc, sent) = open("offline");
        one(&window, "Change hotkey").invoke_accessible_default_action();
        let _ = one(&window, "Left Ctrl");
        key(&ipc, Key::LAlt, true);
        assert_eq!(captures(&sent), [true]);
        let apply = || one(&window, "Apply hotkey").accessible_enabled();
        assert_eq!(apply(), Some(true));
        ipc.deliver(&Event::Link(Link::Offline));
        assert_eq!(apply(), Some(false));
        assert_eq!(
            one(&window, "Left Alt").accessible_description().as_deref(),
            Some("")
        );
        // The accessible default action of the disabled Apply does nothing.
        one(&window, "Apply hotkey").invoke_accessible_default_action();
        assert!(board_open(&window));
        // Online again: capture is asked for once more and Apply is back.
        ipc.deliver(&Event::Link(Link::Online));
        assert_eq!(captures(&sent), [true]);
        assert_eq!(apply(), Some(true));
    }

    #[test]
    fn a_chord_ending_hands_the_focus_to_apply_and_esc_to_cancel() {
        let (window, ipc, _sent) = open("focus-move");
        let state = window.global::<HotkeyState>();
        one(&window, "Change hotkey").invoke_accessible_default_action();
        let _ = one(&window, "Left Ctrl");
        assert_eq!(state.get_move_focus(), 0);
        key(&ipc, Key::F13, true);
        key(&ipc, Key::F13, false);
        // The board's change handler moves the focus; `releasing_a_chord_ends_capture_and_space_
        // applies_it` shows that Space then presses Apply.
        assert_eq!(state.get_move_focus(), 1);
        // Back on the board, Esc alone points at Cancel.
        i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
        one(&window, "F5").mock_single_click(slint::platform::PointerEventButton::Left);
        assert_eq!(state.get_move_focus(), 0);
        key(&ipc, Key::Esc, true);
        assert_eq!(state.get_move_focus(), 2);
    }

    #[test]
    fn focus_loss_ends_capture_and_drops_late_keys() {
        let (window, ipc, sent) = open("focus");
        one(&window, "Change hotkey").invoke_accessible_default_action();
        let _ = one(&window, "Left Ctrl");
        assert_eq!(captures(&sent), [true]);
        window
            .window()
            .dispatch_event(WindowEvent::WindowActiveChanged(false));
        assert_eq!(captures(&sent), [false]);
        key(&ipc, Key::LAlt, true);
        assert_eq!(one(&window, "Left Alt").accessible_checked(), Some(false));
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
        let _ = one(&window, "Left Ctrl");
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
        let _ = one(&window, "Left Ctrl");
        assert_eq!(captures(&sent), [true]);
        window.hide().unwrap();
        drop(window);
        assert_eq!(captures(&sent), [false]);
    }
}
