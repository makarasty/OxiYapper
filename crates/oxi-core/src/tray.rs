//! Tray menu entries; the platform tray draws them, the daemon acts on them.

/// A tray menu entry; the discriminant is its menu command id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    Settings = 1,
    CopyLast = 2,
    Quit = 3,
}
