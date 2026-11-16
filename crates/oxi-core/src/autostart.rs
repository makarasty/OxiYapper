//! What the daemon does to the OS autostart entry at each start and config reload (item 54): the
//! entry follows `system.autostart`, and a moved exe (portable folder moved) is registered again.

use std::path::Path;

/// The one change to make to the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// It already says what the config wants.
    Keep,
    /// Write this command line: the exe path in quotes, so a path with spaces stays one.
    Write(String),
    /// Remove it: autostart is off and the entry starts this exe.
    Remove,
}

/// The change that makes `current` (the entry's command line, `None` when absent) match `enabled`
/// for `exe`. With autostart off, an entry that starts another exe is not this copy's to remove.
pub fn decide(enabled: bool, current: Option<&str>, exe: &Path) -> Action {
    let exe = exe.to_string_lossy();
    let ours = current.is_some_and(|c| target(c).eq_ignore_ascii_case(&exe));
    match (enabled, ours) {
        (true, true) | (false, false) => Action::Keep,
        (true, false) => Action::Write(format!("\"{exe}\"")),
        (false, true) => Action::Remove,
    }
}

/// The program a command line starts: the quoted part, else the whole line.
fn target(command: &str) -> &str {
    let command = command.trim();
    match command.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(rest),
        None => command,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXE: &str = r"C:\Synth Apps\OxiYapper\OxiYapper.exe";

    fn decide_for(enabled: bool, current: Option<&str>) -> Action {
        decide(enabled, current, Path::new(EXE))
    }

    #[test]
    fn on_writes_a_missing_or_moved_entry_and_keeps_its_own() {
        let quoted = format!("\"{EXE}\"");
        assert_eq!(decide_for(true, None), Action::Write(quoted.clone()));
        assert_eq!(
            decide_for(true, Some(r#""D:\Old Place\OxiYapper.exe""#)),
            Action::Write(quoted.clone())
        );
        assert_eq!(decide_for(true, Some(&quoted)), Action::Keep);
        // Case and an argument after the quoted path do not make it another exe.
        let other_case = format!("\"{}\" --x", EXE.to_lowercase());
        assert_eq!(decide_for(true, Some(&other_case)), Action::Keep);
        assert_eq!(decide_for(true, Some("")), Action::Write(quoted));
    }

    #[test]
    fn off_removes_only_an_entry_that_starts_this_exe() {
        assert_eq!(
            decide_for(false, Some(&format!("\"{EXE}\""))),
            Action::Remove
        );
        assert_eq!(decide_for(false, Some(EXE)), Action::Remove);
        assert_eq!(
            decide_for(false, Some(r#""D:\Other\OxiYapper.exe""#)),
            Action::Keep
        );
        assert_eq!(decide_for(false, None), Action::Keep);
    }
}
