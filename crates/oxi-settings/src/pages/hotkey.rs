//! Hotkey page: the chord field, the visual keyboard that picks a new chord, and the recording
//! mode. Keys are clicked on the board or pressed: while the board has the focus the daemon's
//! hook forwards them (`CaptureHotkey`); the page never hooks the keyboard itself.

#[path = "keyboard_layout.rs"]
mod keyboard_layout;

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak as RcWeak};
use std::time::Duration;

use oxi_core::config::OtherKey;
use oxi_core::hotkey::{Chord, ChordError, Key};
use oxi_core::ipc::{Push, Request};
use oxi_core::machine::Mode;
use oxi_core::strings;
use slint::{ComponentHandle, Model, ModelRc, SharedString, Timer, TimerMode, VecModel, Weak};

use crate::debounce::SaveDebounce;
use crate::generated::{HotkeyState, KeyCap};
use crate::ipc::{Event, Link, Subscription};
use crate::{AppWindow, Ctx, Page, report};

/// How often a running capture checks that its page is still shown.
// shortcut: the shell has no page-changed callback and a rail click leaves the focus on the
// board, so leaving the page is polled while capturing; use the callback once the shell has one.
const WATCH: Duration = Duration::from_millis(100);

/// `hotkey.other_key` by the index of its Combo item.
const OTHER_KEYS: [OtherKey; 3] = [OtherKey::Auto, OtherKey::Never, OtherKey::Always];

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let s = window.global::<HotkeyState>();
    let file = ctx.config.borrow();
    set_saved_chord(&s, &file.config.hotkey.chord);
    s.set_mode(i32::from(file.config.hotkey.mode == Mode::Toggle));
    s.set_esc_cancels(file.config.hotkey.esc_cancels);
    let other = OTHER_KEYS
        .iter()
        .position(|&k| k == file.config.hotkey.other_key);
    s.set_other_key(other.and_then(|i| i32::try_from(i).ok()).unwrap_or(0));
    drop(file);
    s.set_columns(keyboard_layout::WIDTH);
    s.set_rows(keyboard_layout::HEIGHT);
    set_link(&s, ctx.ipc.link());

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
    let (w, c) = (window.as_weak(), ctx.clone());
    s.on_esc_cancels_toggled(move |on| {
        c.config.borrow_mut().config.hotkey.esc_cancels = on;
        report(&w, c.save());
    });
    let c = ctx.clone();
    // Arrowing through the Combo picks on every step: save once it rests.
    let save = SaveDebounce::new(window.as_weak(), ctx.clone());
    s.on_other_key_picked(move |i| {
        if let Some(&k) = usize::try_from(i).ok().and_then(|i| OTHER_KEYS.get(i)) {
            c.config.borrow_mut().config.hotkey.other_key = k;
            save.touch();
        }
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
        self.with(|s| set_link(&s, link));
        if self.capturing.get() {
            self.held.borrow_mut().clear();
            if link == Link::Online {
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
        let names = chord.as_ref().map_or_else(
            |_| pending.iter().map(|k| k.as_str().to_owned()).collect(),
            Chord::to_names,
        );
        let error = match chord {
            Ok(_) | Err(ChordError::Empty) => SharedString::new(),
            Err(e) => e.to_string().into(),
        };
        self.with(|s| {
            s.set_pending(chord_text(&names));
            s.set_pending_keys(string_model(&labels(&names)));
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

/// What a person reads for config key names: "[" for `BracketLeft`. Modifiers ("Ctrl") are no
/// `Key` and read as they are.
pub(crate) fn labels(names: &[String]) -> Vec<String> {
    names
        .iter()
        .map(|n| {
            Key::from_name(n)
                .map_or_else(|| n.clone(), |k| keyboard_layout::chip_label(k).to_owned())
        })
        .collect()
}

/// The chord in words for a screen reader and the "+N" tooltip: "Ctrl + Numpad 1", where the
/// caps of "1" and "Num1" read the same.
fn chord_text(names: &[String]) -> SharedString {
    let words: Vec<String> = names
        .iter()
        .map(|n| Key::from_name(n).map_or_else(|| n.clone(), Key::spoken))
        .collect();
    words.join(" + ").into()
}

fn string_model(items: &[String]) -> ModelRc<SharedString> {
    let items: Vec<SharedString> = items.iter().map(SharedString::from).collect();
    ModelRc::from(Rc::new(VecModel::from(items)))
}

/// Apply is on only while online; `note` is its tooltip otherwise.
fn set_link(s: &HotkeyState<'_>, link: Link) {
    s.set_online(link == Link::Online);
    s.set_note(link.off_reason(strings::HOTKEY_NEEDS_DAEMON).into());
}

/// Show the config `names` as the saved chord: the text for screen readers and the keycaps.
fn set_saved_chord(s: &HotkeyState<'_>, names: &[String]) {
    s.set_chord(chord_text(names));
    s.set_chord_keys(string_model(&labels(names)));
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

    use std::rc::Rc;

    use crate::generated::{HotkeyState, Theme};
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
        let default_size = cap(1280.0);
        let middle = cap(1120.0);
        let large = cap(1600.0);
        let huge = cap(2400.0);
        // A cap is the unit less its gap: 28 px units give 25 px caps, 44.39 (the widest window,
        // 1280) give 40.8, 52 give 47.8.
        assert!((small - 25.0).abs() < 0.01, "720: {small}");
        assert!(
            (default_size - 44.39 * 0.92).abs() < 0.5,
            "1280: {default_size}"
        );
        assert!((32.0..36.0).contains(&middle), "1120: {middle}");
        assert!((large - 52.0 * 0.92).abs() < 0.5, "1600: {large}");
        assert!((huge - large).abs() < 0.01, "stopped growing: {huge}");
    }

    #[test]
    fn the_cap_label_size_stays_readable_at_every_unit() {
        let (window, _ipc, _sent) = open("label-size");
        let theme = window.global::<Theme>();
        assert!(
            (theme.invoke_keycap_label_size(28.0) - 12.0).abs() < 0.01,
            "floor"
        );
        assert!(
            (theme.invoke_keycap_label_size(52.0) - 15.0).abs() < 0.01,
            "ceiling"
        );
    }

    /// In the real font (the testing backend's text metrics are fake), so Windows only.
    #[cfg(windows)]
    #[test]
    fn every_label_fits_its_cap_on_one_line() {
        std::thread::spawn(measure_labels).join().unwrap();
    }

    #[cfg(windows)]
    fn measure_labels() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        use slint::platform::{Platform, WindowAdapter};

        slint::slint! {
            export component Measure inherits Window {
                in property <string> family;
                in property <string> label;
                in property <length> size;
                in property <int> weight: 400;
                out property <length> needed: t.preferred-width;
                default-font-family: root.family;
                t := Text {
                    text: root.label;
                    font-size: root.size;
                    font-weight: root.weight;
                    wrap: no-wrap;
                }
            }
        }
        struct Real;
        impl Platform for Real {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
                Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
            }
        }
        slint::platform::set_platform(Box::new(Real)).unwrap();
        let window = crate::open(crate::testing::layout("hotkey", "measure")).unwrap();
        let theme = window.global::<Theme>();
        let measure = Measure::new().unwrap();
        measure.set_family(theme.get_font_family());
        for cap in super::keyboard_layout::caps() {
            // A chord's keycap (`KeycapRow`) is 44 px wide; keep 2 px on each side.
            measure.set_label(super::keyboard_layout::chip_label(cap.key).into());
            measure.set_size(theme.get_body_size());
            measure.set_weight(theme.get_weight_strong());
            assert!(
                measure.get_needed() <= 40.0,
                "{} needs {} px on a chord keycap",
                measure.get_label(),
                measure.get_needed()
            );
            measure.set_weight(400);
            let from = super::keyboard_layout::full_from(cap.key);
            for unit in [28.0, 30.0, 35.9, 36.0, 40.9, 41.0, 43.0, 52.0] {
                let label = super::keyboard_layout::cap_label(cap.key, unit >= from);
                measure.set_label(label.into());
                measure.set_size(theme.invoke_keycap_label_size(unit));
                // The cap less its gap and the 2 px Keycap keeps on each side of the label.
                let room = cap.w * unit - theme.invoke_keycap_gap_for(unit) - 4.0;
                assert!(
                    measure.get_needed() <= room,
                    "{label} needs {} px, has {room} at unit {unit}",
                    measure.get_needed()
                );
            }
        }
    }

    #[test]
    fn twin_keys_read_apart_but_keep_their_caps() {
        let (window, ipc, _sent) = open("twins");
        let state = window.global::<HotkeyState>();
        let caps =
            |m: slint::ModelRc<slint::SharedString>| slint::Model::iter(&m).collect::<Vec<_>>();
        one(&window, "Change hotkey").invoke_accessible_default_action();
        key(&ipc, Key::Digit1, true);
        key(&ipc, Key::Num1, true);
        assert!(one(&window, "New hotkey: 1 + Numpad 1").is_valid());
        assert_eq!(caps(state.get_pending_keys()), ["1", "1"]);
        one(&window, "Apply hotkey").invoke_accessible_default_action();
        assert!(one(&window, "1 + Numpad 1").is_valid());
        assert_eq!(caps(state.get_chord_keys()), ["1", "1"]);
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
