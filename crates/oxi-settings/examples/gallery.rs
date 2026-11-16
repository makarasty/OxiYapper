//! The design-system gallery with a theme switch and a `Shell.locked` switch:
//! `cargo run -p oxi-settings --example gallery`.

// Slint's generated code trips the workspace `unused_qualifications` lint; it is not ours to fix.
#[allow(unused_qualifications)]
mod gallery {
    include!(concat!(env!("OUT_DIR"), "/gallery.rs"));
}

use slint::ComponentHandle;

fn main() -> Result<(), slint::PlatformError> {
    let window = gallery::GalleryWindow::new()?;
    window
        .global::<gallery::PathText>()
        .on_elide(|path, budget| {
            oxi_core::paths::elide_path(&path, usize::try_from(budget).unwrap_or(0)).into()
        });
    window.run()
}
