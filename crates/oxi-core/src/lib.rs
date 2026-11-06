//! Pure logic of oxi-yapper. Rule: no OS calls and no `-sys` crate in the normal
//! dependency tree (CI checks it), so `cargo test -p oxi-core` runs anywhere in seconds.
#![forbid(unsafe_code)]

/// What the executable was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Daemon,
    Settings,
    Version,
}

/// Parses the first command-line argument.
pub fn parse_command(arg: Option<&str>) -> Result<Command, String> {
    match arg {
        None => Ok(Command::Daemon),
        Some("settings") => Ok(Command::Settings),
        Some("--version") => Ok(Command::Version),
        Some(other) => Err(format!(
            "unknown argument: {other}\nusage: oxi-yapper [settings|--version]"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_and_rejects_unknown() {
        assert_eq!(parse_command(None), Ok(Command::Daemon));
        assert_eq!(parse_command(Some("settings")), Ok(Command::Settings));
        assert_eq!(parse_command(Some("--version")), Ok(Command::Version));
        assert!(parse_command(Some("-v")).unwrap_err().contains("usage:"));
    }
}
