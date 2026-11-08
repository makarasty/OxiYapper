//! Every settings screen drawn by the software renderer, at the default size and at the minimum
//! size, light and dark. With `OXI_SHOTS_DIR` set, each lands in
//! `settings-<screen>-<width>-<theme>.png` for visual review.

mod common;

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_core::device::Device;
use oxi_core::hotkey::Key;
use oxi_core::ipc::{DownloadResult, Push};
use oxi_core::paths::Layout;
use oxi_settings::ipc::{Event, Link};
use oxi_settings::{AppWindow, Ipc, Page, ThemeChoice};
use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
};
use slint::platform::{Platform, WindowAdapter, WindowEvent};
use slint::{Color, ComponentHandle, PhysicalSize, PlatformError};
use tiny_skia::{IntSize, Pixmap};

/// The default window size (spec section 2 on a large screen) and the minimum.
const DEFAULT: (f32, f32) = (1280.0, 800.0);
const MIN: (f32, f32) = (720.0, 520.0);
const SIZES: [(f32, f32); 2] = [DEFAULT, MIN];
const THEMES: [(&str, ThemeChoice); 2] =
    [("light", ThemeChoice::Light), ("dark", ThemeChoice::Dark)];
const PAGES: [(&str, Page); 7] = [
    ("hotkey", Page::Hotkey),
    ("audio", Page::Audio),
    ("model", Page::Model),
    ("text", Page::Text),
    ("overlay", Page::Overlay),
    ("system", Page::System),
    ("about", Page::About),
];

thread_local! {
    /// The window of the component created last on this thread.
    static LAST: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

struct Shots;

thread_local! {
    /// The size `matrix` renders next; the window takes it when created, because the element
    /// search finds only what is in view.
    static SIZE: std::cell::Cell<(f32, f32)> = const { std::cell::Cell::new(DEFAULT) };
}

impl Platform for Shots {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        let (w, h) = SIZE.get();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
        window.set_size(PhysicalSize::new(w as u32, h as u32));
        LAST.with_borrow_mut(|last| *last = Some(window.clone()));
        Ok(window)
    }

    // Timers (the model page's path check) run on this clock; the default one stands still.
    fn duration_since_start(&self) -> std::time::Duration {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed()
    }
}

fn open(screen: &str, config: Option<&str>) -> AppWindow {
    open_with(screen, config, |_| {})
}

/// `prepare` runs on the fresh layout before the window reads it.
fn open_with(screen: &str, config: Option<&str>, prepare: fn(&Layout)) -> AppWindow {
    // Once per thread; a second call on the same thread is the same platform.
    let _ = slint::platform::set_platform(Box::new(Shots));
    let layout = common::layout(screen, config);
    prepare(&layout);
    oxi_settings::open(layout).unwrap()
}

/// `window` at `size` logical px and `scale`.
fn render(window: &AppWindow, size: (f32, f32), scale: f32) -> Pixmap {
    let surface = LAST.with_borrow(Clone::clone).unwrap();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
    let (w, h) = ((size.0 * scale) as u32, (size.1 * scale) as u32);
    surface.dispatch_event(WindowEvent::ScaleFactorChanged {
        scale_factor: scale,
    });
    surface.set_size(PhysicalSize::new(w, h));
    window.show().unwrap();
    // Runs the `changed` handlers (the theme) and settles animations.
    slint::platform::update_timers_and_animations();
    surface.request_redraw();
    let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
    assert!(surface.draw_if_needed(|renderer| {
        renderer.render(&mut buffer, w as usize);
    }));
    let bytes = buffer
        .iter()
        .flat_map(|p| [p.red, p.green, p.blue, p.alpha])
        .collect();
    Pixmap::from_vec(bytes, IntSize::from_wh(w, h).unwrap()).unwrap()
}

fn save(pixmap: &Pixmap, name: &str) {
    let pixels = pixmap.pixels();
    assert!(pixels.iter().any(|p| p.alpha() > 0), "{name} drew nothing");
    assert!(
        pixels.iter().any(|p| *p != pixels[0]),
        "{name} is one flat colour"
    );
    if let Some(dir) = std::env::var_os("OXI_SHOTS_DIR") {
        let dir = PathBuf::from(dir);
        fs::create_dir_all(&dir).unwrap();
        pixmap
            .save_png(dir.join(format!("settings-{name}.png")))
            .unwrap();
    }
}

/// `screen` at both sizes in both themes; `make` opens the window in the theme it gets.
fn matrix(screen: &str, make: impl Fn(ThemeChoice) -> AppWindow) {
    for size in SIZES {
        for (theme, choice) in THEMES {
            SIZE.set(size);
            let window = make(choice);
            save(
                &render(&window, size, 1.0),
                &format!("{screen}-{}-{theme}", size.0),
            );
        }
    }
}

fn shot(screen: &str, page: Page, config: Option<&str>) {
    shot_with(screen, page, config, |_| {});
}

fn shot_with(screen: &str, page: Page, config: Option<&str>, prepare: fn(&Layout)) {
    matrix(screen, |choice| {
        let window = open_with(screen, config, prepare);
        window.set_page(page);
        window.set_theme(choice);
        window
    });
}

const NEWER: &str = "version = 99\n";
const DENOISE: &str = "version = 1
[audio]
denoise = true
";
const BROKEN: &str = "version = 1\n[audio]\ngain_db = \n";

#[test]
fn shot_main() {
    shot("main", Page::Hotkey, None);
}

#[test]
fn shot_main_scaled() {
    let window = open("main-1.5x", None);
    let pixmap = render(&window, DEFAULT, 1.5);
    assert_eq!((pixmap.width(), pixmap.height()), (1920, 1200));
    save(&pixmap, "main-1.5x-system");
}

#[test]
fn shot_pages_scaled() {
    for (screen, page) in PAGES {
        let window = open("pages-1.5x", None);
        window.set_page(page);
        window.set_theme(ThemeChoice::Light);
        save(
            &render(&window, DEFAULT, 1.5),
            &format!("{screen}-1.5x-light"),
        );
    }
}

#[test]
fn shot_large() {
    // The column stops at 880 px and the extra width stays empty on the right.
    for (theme, choice) in THEMES {
        for (screen, page) in [("audio", Page::Audio), ("hotkey", Page::Hotkey)] {
            let window = open("large", None);
            window.set_page(page);
            window.set_theme(choice);
            save(
                &render(&window, (1600.0, 1000.0), 1.0),
                &format!("{screen}-1600-{theme}"),
            );
        }
    }
}

#[test]
fn shot_focused_nav() {
    // Tab, then Down twice: the keyboard focus ring sits on Model, Hotkey stays selected.
    use slint::platform::Key as K;
    for (theme, choice) in THEMES {
        for size in SIZES {
            let window = open("focused-nav", None);
            window.set_theme(choice);
            let _ = render(&window, size, 1.0);
            let surface = LAST.with_borrow(Clone::clone).unwrap();
            for key in [K::Tab, K::DownArrow, K::DownArrow] {
                surface.dispatch_event(WindowEvent::KeyPressed { text: key.into() });
                surface.dispatch_event(WindowEvent::KeyReleased { text: key.into() });
            }
            save(
                &render(&window, size, 1.0),
                &format!("focused-nav-{}-{theme}", size.0),
            );
        }
    }
}

/// A pixel of the purple system accent family: HSV hue 260..300 with chroma over 0.08 (the
/// same test as `gallery.rs`).
fn purple(r: u8, g: u8, b: u8) -> bool {
    let (r, g, b) = (f32::from(r), f32::from(g), f32::from(b));
    let max = r.max(g).max(b);
    let d = max - r.min(g).min(b);
    if d / 255.0 <= 0.08 {
        return false;
    }
    let hue = if (max - r).abs() < f32::EPSILON {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if (max - g).abs() < f32::EPSILON {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (260.0..=300.0).contains(&hue)
}

fn purple_pixels(pixmap: &Pixmap) -> usize {
    pixmap
        .pixels()
        .iter()
        .map(tiny_skia::PremultipliedColorU8::demultiply)
        .filter(|p| purple(p.red(), p.green(), p.blue()))
        .count()
}

const PURPLE: Color = Color::from_rgb_u8(0x87, 0x64, 0xB8);

#[test]
fn no_system_accent_on_any_page() {
    // Windows reports a purple accent before the window opens: the window puts its teal over it.
    // The first window sets up Slint, which the accent call needs.
    drop(open("purple-setup", None));
    for (theme, choice) in THEMES {
        for (screen, page) in PAGES {
            i_slint_backend_testing::set_system_accent_color(PURPLE);
            let window = open("purple", None);
            window.set_page(page);
            window.set_theme(choice);
            let pixmap = render(&window, DEFAULT, 1.0);
            save(&pixmap, &format!("purple-{screen}-{theme}"));
            let leaks = purple_pixels(&pixmap);
            assert_eq!(leaks, 0, "{leaks} purple pixels on {screen} ({theme})");
        }
    }
}

#[test]
fn a_later_accent_change_is_undone_by_the_poll() {
    // A focused std field draws the context accent, so a purple accent shows on it until the next
    // file poll puts the teal back. Tab twice: the rail, then the first editor.
    use slint::platform::Key as K;
    let window = open("purple-later", None);
    window.set_page(Page::Text);
    window.set_theme(ThemeChoice::Light);
    let _ = render(&window, DEFAULT, 1.0);
    let surface = LAST.with_borrow(Clone::clone).unwrap();
    for _ in 0..2 {
        surface.dispatch_event(WindowEvent::KeyPressed {
            text: K::Tab.into(),
        });
        surface.dispatch_event(WindowEvent::KeyReleased {
            text: K::Tab.into(),
        });
    }
    i_slint_backend_testing::set_system_accent_color(PURPLE);
    assert!(
        purple_pixels(&render(&window, DEFAULT, 1.0)) > 0,
        "the check sees the system accent"
    );
    std::thread::sleep(oxi_settings::FILE_POLL + std::time::Duration::from_millis(100));
    slint::platform::update_timers_and_animations();
    assert_eq!(purple_pixels(&render(&window, DEFAULT, 1.0)), 0);
}

#[test]
fn shot_about() {
    shot("about", Page::About, None);
}

#[test]
fn shot_banner_error() {
    shot("banner_error", Page::Hotkey, Some(BROKEN));
}

#[test]
fn shot_banner_readonly() {
    shot("banner_readonly", Page::Hotkey, Some(NEWER));
}

#[test]
fn shot_hotkey() {
    shot("hotkey", Page::Hotkey, None);
}

/// The hotkey page with its keyboard open on a fake daemon link; `drive` presses keys or drops the
/// link before the shot.
fn shot_board(screen: &str, drive: fn(&Ipc)) {
    matrix(screen, |choice| {
        let _ = slint::platform::set_platform(Box::new(Shots));
        let (ipc, _sent) = Ipc::fake();
        let window = oxi_settings::open_with(common::layout(screen, None), ipc.clone()).unwrap();
        window.set_page(Page::Hotkey);
        window.set_theme(choice);
        window.show().unwrap();
        ElementHandle::find_by_accessible_label(&window, "Change hotkey")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        drive(&ipc);
        window
    });
}

fn key(ipc: &Ipc, code: Key, down: bool) {
    push(ipc, Push::Key { code, down });
}

#[test]
fn shot_keyboard() {
    // Open with nothing pressed.
    shot_board("keyboard", |_| {});
}

#[test]
fn shot_keyboard_held() {
    // Ctrl + Shift + F13 down: held keys lit.
    shot_board("keyboard-held", |ipc| {
        for k in [Key::LCtrl, Key::LShift, Key::F13] {
            key(ipc, k, true);
        }
    });
}

#[test]
fn shot_keyboard_invalid() {
    // Esc inside a chord: the error line.
    shot_board("keyboard-invalid", |ipc| {
        key(ipc, Key::LCtrl, true);
        key(ipc, Key::Esc, true);
    });
}

#[test]
fn shot_keyboard_ready() {
    // Every key released after Ctrl + F13: the focus moves to Apply.
    shot_board("keyboard-ready", |ipc| {
        for k in [Key::LCtrl, Key::F13] {
            key(ipc, k, true);
        }
        for k in [Key::LCtrl, Key::F13] {
            key(ipc, k, false);
        }
    });
}

#[test]
fn shot_keyboard_large() {
    // The board at the widest window it grows to: Ctrl + F13 picked, Space down.
    for (theme, choice) in THEMES {
        SIZE.set((1600.0, 1000.0));
        let _ = slint::platform::set_platform(Box::new(Shots));
        let (ipc, _sent) = Ipc::fake();
        let layout = common::layout("keyboard-large", None);
        let window = oxi_settings::open_with(layout, ipc.clone()).unwrap();
        window.set_page(Page::Hotkey);
        window.set_theme(choice);
        window.show().unwrap();
        ElementHandle::find_by_accessible_label(&window, "Change hotkey")
            .next()
            .unwrap()
            .invoke_accessible_default_action();
        for k in [Key::LCtrl, Key::F13] {
            key(&ipc, k, true);
        }
        key(&ipc, Key::F13, false);
        key(&ipc, Key::Space, true);
        save(
            &render(&window, (1600.0, 1000.0), 1.0),
            &format!("keyboard-1600-{theme}"),
        );
    }
}

#[test]
fn shot_keyboard_offline() {
    // The daemon goes away with the board open.
    shot_board("keyboard-offline", |ipc| {
        key(ipc, Key::LCtrl, true);
        ipc.deliver(&Event::Link(Link::Offline));
    });
}

#[test]
fn shot_audio() {
    shot("audio", Page::Audio, None);
}

#[test]
fn shot_audio_denoise() {
    shot("audio-denoise", Page::Audio, Some(DENOISE));
}

// The not-connected, level and clipping states are `shots_audio` in `pages::audio`: they list the
// devices through a fake, where this binary could only ask the real sound system.

#[test]
fn shot_model() {
    shot("model", Page::Model, None);
}

/// A model page on a fake daemon link: `prepare` fills the layout, `drive` pushes what the daemon
/// would send once the page is up.
fn shot_model_linked(screen: &str, config: &str, prepare: fn(&Layout), drive: fn(&Ipc)) {
    matrix(screen, |choice| {
        let _ = slint::platform::set_platform(Box::new(Shots));
        let layout = common::layout(screen, Some(config));
        prepare(&layout);
        let (ipc, _sent) = Ipc::fake();
        let window = oxi_settings::open_with(layout, ipc.clone()).unwrap();
        window.set_page(Page::Model);
        window.set_theme(choice);
        // Showing builds the page, whose `init` starts the path check again.
        window.show().unwrap();
        drive(&ipc);
        // The path check answers from its worker thread; let the page's poll timer take it.
        for _ in 0..40 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            slint::platform::update_timers_and_animations();
        }
        window
    });
}

fn push(ipc: &Ipc, push: Push) {
    ipc.deliver(&Event::Push(push));
}

const BUSY: &str = "version = 1
[model]
id = \"turbo\"
gpu = \"Vulkan0\"
";

#[test]
fn shot_model_busy() {
    // Downloading with progress (selected), a failed download, a paused one, the GPU named.
    shot_model_linked(
        "model-busy",
        BUSY,
        |l| {
            let dir = l.models_dir();
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("ggml-large-v3-russian-ties-podlodka-v1.2-f16.bin.part"),
                b"half",
            )
            .unwrap();
        },
        |ipc| {
            push(
                ipc,
                Push::Devices {
                    mics: vec![],
                    gpus: vec![Device {
                        name: "Vulkan0".into(),
                        description: "NVIDIA GeForce RTX 4070 SUPER".into(),
                        uma: false,
                    }],
                },
            );
            push(
                ipc,
                Push::DownloadProgress {
                    id: "turbo".into(),
                    done: 650_000_000,
                    total: 1_624_555_275,
                },
            );
            push(
                ipc,
                Push::DownloadDone {
                    id: "large-v3".into(),
                    result: DownloadResult::Failed {
                        message: "connection reset (os error 10054)".into(),
                    },
                },
            );
        },
    );
}

#[test]
fn shot_model_downloaded() {
    // Ready (selected), downloaded but unchecked, and a damaged file.
    shot_model_linked(
        "model-downloaded",
        BUSY,
        |l| {
            let dir = l.models_dir();
            fs::create_dir_all(&dir).unwrap();
            for (name, size) in [
                ("ggml-large-v3-turbo.bin", 1_624_555_275),
                ("ggml-large-v3.bin", 3_095_033_483),
                ("ggml-large-v3-russian-ties-podlodka-v1.2-f16.bin", 1000),
            ] {
                let file = dir.join(name);
                fs::File::create(&file).unwrap().set_len(size).unwrap();
                if name.ends_with("turbo.bin") {
                    oxi_core::models::write_stamp(&file).unwrap();
                }
            }
        },
        |_| {},
    );
}

#[test]
fn shot_model_custom_relative() {
    shot_model_linked(
        "model-custom-relative",
        "version = 1
[model]
id = \"custom\"
custom_path = \"mine.bin\"
",
        |_| {},
        |_| {},
    );
}

#[test]
fn shot_model_custom_missing() {
    shot_model_linked(
        "model-custom-missing",
        "version = 1
[model]
id = \"custom\"
custom_path = \"C:/nowhere/gone.bin\"
",
        |_| {},
        |_| {},
    );
}

#[test]
fn shot_model_offline() {
    shot_model_linked(
        "model-offline",
        BUSY,
        |_| {},
        |ipc| {
            ipc.deliver(&Event::Link(Link::Offline));
        },
    );
}

#[test]
fn shot_text() {
    shot("text", Page::Text, None);
}

#[test]
fn shot_text_notes() {
    // Over the token budget (red count and note), a line without `=>`, and the broken-config banner.
    shot_with("text-notes", Page::Text, Some(BROKEN), |l| {
        let words: Vec<String> = (0..400).map(|i| format!("word{i}")).collect();
        fs::write(
            l.dictionary_file(),
            words.join(
                "
",
            ),
        )
        .unwrap();
        fs::write(
            l.replace_file(),
            "heard => wanted
no arrow here
",
        )
        .unwrap();
    });
}

#[test]
fn shot_text_unreadable() {
    // A directory where replace.txt belongs: reading it fails.
    shot_with("text-unreadable", Page::Text, None, |l| {
        fs::create_dir(l.replace_file()).unwrap();
    });
}

#[test]
fn shot_text_conflict() {
    // Unsaved edits against a file changed on disk: the conflict note with Reload and Overwrite.
    matrix("text-conflict", |choice| {
        let _ = slint::platform::set_platform(Box::new(Shots));
        let layout = common::layout("text-conflict", None);
        fs::write(layout.dictionary_file(), "synthalpha").unwrap();
        let window = oxi_settings::open(layout.clone()).unwrap();
        window.set_page(Page::Text);
        window.set_theme(choice);
        window.show().unwrap();
        ElementHandle::find_by_accessible_label(&window, "Dictionary")
            .find(|e| e.accessible_role() == Some(AccessibleRole::TextInput))
            .unwrap()
            .set_accessible_value("synthbeta");
        fs::write(layout.dictionary_file(), "synthgamma").unwrap();
        common::one(&window, "Save dictionary.txt").invoke_accessible_default_action();
        window
    });
}

#[test]
fn shot_text_worst_case() {
    // Daemon banner, both files unreadable (a note each), three rules and the duplicate note: more
    // than the window holds, so the page scrolls; Add must be reachable by wheel and by Tab.
    // Items outside the viewport are not in the element search, so "found" means "in view".
    use slint::platform::Key as Tab;
    for (theme, choice) in THEMES {
        let _ = slint::platform::set_platform(Box::new(Shots));
        let layout = common::layout("text-worst", None);
        fs::create_dir(layout.dictionary_file()).unwrap();
        fs::create_dir(layout.replace_file()).unwrap();
        let (ipc, _sent) = Ipc::fake();
        let window = oxi_settings::open_with(layout, ipc.clone()).unwrap();
        window.set_page(Page::Text);
        window.set_theme(choice);
        window.show().unwrap();
        ipc.deliver(&Event::Link(Link::Offline));
        let surface = LAST.with_borrow(Clone::clone).unwrap();
        let _ = render(&window, MIN, 1.0);
        let wheel = |dy: f32| {
            surface.dispatch_event(WindowEvent::PointerScrolled {
                position: slint::LogicalPosition::new(280.0, 300.0),
                delta_x: 0.0,
                delta_y: dy,
            });
            slint::platform::update_timers_and_animations();
        };
        // The Add button, when it is on screen.
        let add_in_view = || {
            ElementHandle::find_by_accessible_label(&window, "Add rule")
                .next()
                .is_some_and(|e| e.absolute_position().y + e.size().height <= MIN.1)
        };
        assert!(
            !add_in_view(),
            "the worst case should not fit: no scrolling to test"
        );
        wheel(-2000.0);
        assert!(add_in_view(), "Add not reachable by wheel");
        for app in ["a.exe", "b.exe", "A.EXE"] {
            wheel(-2000.0);
            common::one(&window, "New rule app").set_accessible_value(app);
            common::one(&window, "Add rule").invoke_accessible_default_action();
        }
        wheel(-2000.0);
        assert!(common::has(
            &window,
            "A.EXE already has a rule: change its mode in the list above."
        ));
        save(
            &render(&window, MIN, 1.0),
            &format!("text-worst-bottom-720-{theme}"),
        );
        wheel(2000.0);
        save(
            &render(&window, MIN, 1.0),
            &format!("text-worst-top-720-{theme}"),
        );
        // Keyboard only: from the top, Tab until Add is on screen.
        wheel(-2000.0);
        common::one(&window, "New rule app").set_accessible_value("c.exe");
        wheel(2000.0);
        assert!(!add_in_view());
        let mut tabs = 0;
        while !add_in_view() && tabs < 60 {
            surface.dispatch_event(WindowEvent::KeyPressed {
                text: Tab::Tab.into(),
            });
            surface.dispatch_event(WindowEvent::KeyReleased {
                text: Tab::Tab.into(),
            });
            slint::platform::update_timers_and_animations();
            tabs += 1;
        }
        assert!(add_in_view(), "Tab never brought Add into view");
        save(
            &render(&window, MIN, 1.0),
            &format!("text-worst-tabbed-720-{theme}"),
        );
    }
}

#[test]
fn shot_overlay() {
    shot("overlay", Page::Overlay, None);
}

#[test]
fn shot_overlay_narrow() {
    // Under 640 px of content the Preview goes below the picker; tall enough to show it.
    for (theme, choice) in THEMES {
        let window = open("overlay-narrow", None);
        window.set_page(Page::Overlay);
        window.set_theme(choice);
        save(
            &render(&window, (720.0, 900.0), 1.0),
            &format!("overlay-narrow-{theme}"),
        );
    }
}

#[test]
fn shot_system() {
    shot("system", Page::System, None);
}

#[test]
fn shot_system_update_off() {
    // Turning the update check off asks for a confirmation first.
    matrix("system-update-off", |choice| {
        let window = open("system-update-off", None);
        window.set_page(Page::System);
        window.set_theme(choice);
        window.show().unwrap();
        ElementHandle::find_by_accessible_label(&window, "Check for updates")
            .find(|e| e.accessible_checkable() == Some(true))
            .unwrap()
            .invoke_accessible_default_action();
        window
    });
}

#[test]
fn shot_overlay_mismatch() {
    // An old daemon still runs after an update: Preview is off and says to restart.
    matrix("overlay-mismatch", |choice| {
        let _ = slint::platform::set_platform(Box::new(Shots));
        let (ipc, _sent) = Ipc::fake();
        let layout = common::layout("overlay-mismatch", None);
        let window = oxi_settings::open_with(layout, ipc.clone()).unwrap();
        window.set_page(Page::Overlay);
        window.set_theme(choice);
        window.show().unwrap();
        ipc.deliver(&Event::Link(Link::Mismatch { daemon_proto: 9 }));
        window
    });
}

#[test]
fn tile_icon_renders_inside_its_card() {
    let window = open("thumb", None);
    window.set_page(Page::System);
    window.set_theme(ThemeChoice::Light);
    let pixmap = render(&window, DEFAULT, 1.0);
    let card = ElementHandle::find_by_accessible_label(&window, "Dark")
        .find(|e| e.accessible_role() == Some(AccessibleRole::RadioButton))
        .unwrap();
    let (pos, size) = (card.absolute_position(), card.size());
    // The ink of the icon and title inside the card, on a light page with no other dark fill.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // logical px at scale 1
    let dark = (pos.y as u32..(pos.y + size.height) as u32)
        .flat_map(|y| (pos.x as u32..(pos.x + size.width) as u32).map(move |x| (x, y)))
        .filter_map(|(x, y)| pixmap.pixel(x, y))
        .filter(|p| p.red() < 0x40 && p.green() < 0x40 && p.blue() < 0x40)
        .count();
    assert!(dark > 60, "{dark} dark pixels in the Dark card");
}
