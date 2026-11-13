//! About page: the Slint tile opens the std "Made with Slint" badge (R2.3, item 9), and every
//! Built-with mark sits in its box at equal insets (item 11, DS-15.4).

mod common;

use std::time::Duration;

use common::{has, layout, open, with_role};
use i_slint_backend_testing::{AccessibleRole, ElementHandle};
use oxi_settings::Page;
use slint::ComponentHandle;

fn settle() {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(1));
}

/// The badge is not on the page; it shows once the Slint tile is clicked, so the royalty-free
/// licence's `AboutSlint` stays reachable in the app.
#[test]
fn the_slint_tile_opens_the_made_with_slint_badge() {
    let window = open(&layout("about-badge", None), Page::About);
    settle();
    assert!(
        !has(&window, "#MadeWithSlint"),
        "the badge is still on the page"
    );
    with_role(&window, "Slint", AccessibleRole::Button).invoke_accessible_default_action();
    settle();
    assert!(
        has(&window, "#MadeWithSlint"),
        "the Slint tile opened no badge"
    );
}

/// DS-24.1: `open-external` means "opens another app". Every link tile carries it; the Slint tile,
/// which opens the in-app badge, carries none.
#[test]
fn only_link_tiles_carry_the_open_external_glyph() {
    let window = open(&layout("about-glyph", None), Page::About);
    settle();
    let tiles: Vec<_> = ElementHandle::find_by_element_type_name(&window, "TechCard").collect();
    let glyphs: Vec<_> =
        ElementHandle::find_by_element_id(&window, "TechCard::external-mark").collect();
    assert_eq!(glyphs.len(), tiles.len() - 1, "one glyph per link tile");
    let slint = rect(&with_role(&window, "Slint", AccessibleRole::Button));
    for g in &glyphs {
        let [x, y, _, _] = rect(g);
        let inside =
            x >= slint[0] && x < slint[0] + slint[2] && y >= slint[1] && y < slint[1] + slint[3];
        assert!(!inside, "the Slint tile shows the open-external glyph");
    }
}

/// Item 11 (DS-15.4): every mark box sits as far from its tile's left edge as from its top and
/// bottom, within 0.5 px, tall and short tiles alike; and every mark is centred in its box.
#[test]
fn every_mark_box_sits_at_equal_insets_in_its_tile() {
    let window = open(&layout("about-insets", None), Page::About);
    for (w, h) in [(720.0_f32, 520.0_f32), (1280.0, 860.0), (1920.0, 1080.0)] {
        window.window().set_size(slint::LogicalSize::new(w, h));
        settle();
        let tiles: Vec<_> = ElementHandle::find_by_element_type_name(&window, "TechCard").collect();
        let boxes: Vec<_> =
            ElementHandle::find_by_element_id(&window, "TechCard::mark-box").collect();
        let marks: Vec<_> = ElementHandle::find_by_element_id(&window, "TechCard::mark").collect();
        assert_eq!(tiles.len(), 9, "{w}x{h}: tiles");
        assert_eq!(boxes.len(), tiles.len(), "{w}x{h}: boxes");
        assert_eq!(marks.len(), tiles.len(), "{w}x{h}: marks");
        for ((t, b), m) in tiles.iter().zip(&boxes).zip(&marks) {
            let (tile, inner) = (rect(t), rect(b));
            let [left, _, top, bottom] = insets(tile, inner);
            let off = spread(&[left, top, bottom]);
            assert!(
                off <= 0.5 && left >= 0.0,
                "{w}x{h}: box {left} from the tile's left, {top} from its top, {bottom} from its bottom"
            );
            let around = insets(inner, rect(m));
            assert!(
                spread(&around) <= 0.5 && around.iter().all(|i| *i >= 0.0),
                "{w}x{h}: mark insets in its box {around:?}"
            );
        }
    }
}

/// An element's (x, y, width, height) in the window.
fn rect(e: &ElementHandle) -> [f32; 4] {
    let (p, s) = (e.absolute_position(), e.size());
    [p.x, p.y, s.width, s.height]
}

/// Left, right, top and bottom of `inner` inside `outer`.
fn insets(outer: [f32; 4], inner: [f32; 4]) -> [f32; 4] {
    let [ox, oy, ow, oh] = outer;
    let [x, y, w, h] = inner;
    [x - ox, ox + ow - (x + w), y - oy, oy + oh - (y + h)]
}

fn spread(v: &[f32]) -> f32 {
    v.iter().copied().fold(f32::MIN, f32::max) - v.iter().copied().fold(f32::MAX, f32::min)
}
