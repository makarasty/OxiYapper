//! Paste vocabulary shared by the paste worker and the state machine (ARCHITECTURE 10).

/// How the paste worker delivered a job's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasteOutcome {
    /// Clipboard set, paste chord sent, clipboard restored.
    Pasted,
    /// Typed key by key (type mode, or the clipboard was locked).
    Typed,
    /// Left on the clipboard for the user to paste.
    ClipboardOnly(ClipboardOnlyWhy),
    /// Pasted, but the clipboard snapshot was incomplete, so the old one was not restored.
    PastedNoRestore,
    /// Nothing landed: `SendInput` inserted no events, a secure desktop, or setting the
    /// clipboard failed. The coordinator keeps the text for "Copy last text" (item 42).
    Failed,
}

/// Why the paste worker did not send the paste chord itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOnlyWhy {
    /// Modifiers still held after 1.5 s (item 35).
    ModifiersHeld,
    /// The target window runs elevated (item 36).
    Elevated,
}
