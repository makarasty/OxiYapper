//! Every settings screen drawn by the software renderer, the one the app ships with. With
//! `OXI_SHOTS_DIR` set, each lands in `settings-<screen>-<theme>.png` for visual review.

mod common;

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_settings::{AppWindow, Page, ThemeChoice};
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
}

fn open(screen: &str, config: Option<&str>) -> AppWindow {
    // Once per thread; a second call on the same thread is the same platform.
    let _ = slint::platform::set_platform(Box::new(Shots));
    let layout = common::layout(screen, config);
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
    for (theme, choice) in [("light", ThemeChoice::Light), ("dark", ThemeChoice::Dark)] {
        let window = open(screen, config);
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

#[test]
fn shot_keyboard() {
    // shortcut: the hotkey page until its task adds the visual keyboard; open it here then.
    shot("keyboard", Page::Hotkey, None);
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
fn shot_audio_not_connected() {
    shot("audio-not-connected", Page::Audio, Some(NO_MIC));
}

#[test]
fn shot_model() {
    shot("model", Page::Model, None);
}

#[test]
fn shot_text() {
    shot("text", Page::Text, None);
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
