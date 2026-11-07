//! Every settings screen drawn by the software renderer, the one the app ships with. With
//! `OXI_SHOTS_DIR` set, each lands in `settings-<screen>-<theme>.png` for visual review.

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
use slint::{ComponentHandle, PhysicalSize, PlatformError};
use tiny_skia::{IntSize, Pixmap};

const WIDTH: f32 = 560.0;
const HEIGHT: f32 = 600.0;

thread_local! {
    /// The window of the component created last on this thread.
    static LAST: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
}

struct Shots;

impl Platform for Shots {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
        let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
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

fn render(window: &AppWindow, scale: f32) -> Pixmap {
    let surface = LAST.with_borrow(Clone::clone).unwrap();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // whole positive pixels
    let (w, h) = ((WIDTH * scale) as u32, (HEIGHT * scale) as u32);
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

fn save(pixmap: &Pixmap, screen: &str, theme: &str) {
    let pixels = pixmap.pixels();
    assert!(
        pixels.iter().any(|p| *p != pixels[0]),
        "{screen} {theme} is one flat colour"
    );
    if let Some(dir) = std::env::var_os("OXI_SHOTS_DIR") {
        let dir = PathBuf::from(dir);
        fs::create_dir_all(&dir).unwrap();
        pixmap
            .save_png(dir.join(format!("settings-{screen}-{theme}.png")))
            .unwrap();
    }
}

/// `screen` in light and in dark.
fn shot(screen: &str, page: Page, config: Option<&str>) {
    shot_with(screen, page, config, |_| {});
}

fn shot_with(screen: &str, page: Page, config: Option<&str>, prepare: fn(&Layout)) {
    for (theme, choice) in [("light", ThemeChoice::Light), ("dark", ThemeChoice::Dark)] {
        let window = open_with(screen, config, prepare);
        window.set_page(page);
        window.set_theme(choice);
        save(&render(&window, 1.0), screen, theme);
    }
}

const NEWER: &str = "version = 99\n";
const DENOISE: &str = "version = 1
[audio]
denoise = true
";
const NO_MIC: &str = "version = 1
[audio]
device = \"Missing Test Mic\"
";
const BROKEN: &str = "version = 1\n[audio]\ngain_db = \n";

#[test]
fn shot_main() {
    shot("main", Page::Hotkey, None);
}

#[test]
fn shot_main_scaled() {
    let window = open("main-1.5x", None);
    let pixmap = render(&window, 1.5);
    assert_eq!((pixmap.width(), pixmap.height()), (840, 900));
    save(&pixmap, "main-1.5x", "system");
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
    for (theme, choice) in [("light", ThemeChoice::Light), ("dark", ThemeChoice::Dark)] {
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
        save(&render(&window, 1.0), screen, theme);
    }
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

#[test]
fn shot_audio_config_missing() {
    shot("audio-config-missing", Page::Audio, Some(NO_MIC));
}

#[test]
fn shot_model() {
    shot("model", Page::Model, None);
}

/// A model page on a fake daemon link: `prepare` fills the layout, `drive` pushes what the daemon
/// would send once the page is up.
fn shot_model_linked(screen: &str, config: &str, prepare: fn(&Layout), drive: fn(&Ipc)) {
    for (theme, choice) in [("light", ThemeChoice::Light), ("dark", ThemeChoice::Dark)] {
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
        save(&render(&window, 1.0), screen, theme);
    }
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
fn shot_overlay() {
    shot("overlay", Page::Overlay, None);
}

#[test]
fn shot_system() {
    shot("system", Page::System, None);
}

#[test]
fn svg_thumbnail_renders_inside_its_card() {
    let window = open("thumb", None);
    window.set_page(Page::System);
    window.set_theme(ThemeChoice::Light);
    let pixmap = render(&window, 1.0);
    let card = ElementHandle::find_by_accessible_label(&window, "Dark")
        .find(|e| e.accessible_role() == Some(AccessibleRole::RadioButton))
        .unwrap();
    let (pos, size) = (card.absolute_position(), card.size());
    // The dark mini-window fill (#1c1c1c) inside the card, on a light page with no other dark fill.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // logical px at scale 1
    let dark = (pos.y as u32..(pos.y + size.height) as u32)
        .flat_map(|y| (pos.x as u32..(pos.x + size.width) as u32).map(move |x| (x, y)))
        .filter_map(|(x, y)| pixmap.pixel(x, y))
        .filter(|p| p.red() < 0x30 && p.green() < 0x30 && p.blue() < 0x30)
        .count();
    assert!(dark > 500, "{dark} dark pixels in the Dark card");
}
