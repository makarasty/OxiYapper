//! Command-line parsing for the `OxiYapper` executable.

/// What the executable was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Daemon,
    Settings,
    Version,
    /// Hidden `bench <args...>`: every following argument, flags included, unparsed.
    Bench(Vec<String>),
}

/// An argument the executable does not know; the message ends with the usage line.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("unknown argument: {0}\nusage: {app} [settings|--version]", app = crate::APP_NAME)]
pub struct UsageError(pub String);

/// Parses the command-line arguments after the program name.
pub fn parse_command(args: &[String]) -> Result<Command, UsageError> {
    match args.first().map(String::as_str) {
        None => Ok(Command::Daemon),
        Some("settings") => Ok(Command::Settings),
        Some("--version") => Ok(Command::Version),
        Some("bench") => Ok(Command::Bench(args[1..].to_vec())),
        Some(other) => Err(UsageError(other.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn parses_known_and_rejects_unknown() {
        assert_eq!(parse_command(&args(&[])), Ok(Command::Daemon));
        assert_eq!(parse_command(&args(&["settings"])), Ok(Command::Settings));
        assert_eq!(parse_command(&args(&["--version"])), Ok(Command::Version));
        assert!(
            parse_command(&args(&["-v"]))
                .unwrap_err()
                .to_string()
                .contains("usage:")
        );
    }

    #[test]
    fn bench_takes_every_following_argument_raw() {
        assert_eq!(parse_command(&args(&["bench"])), Ok(Command::Bench(vec![])));
        assert_eq!(
            parse_command(&args(&["bench", "--model", "x", "-n", "3"])),
            Ok(Command::Bench(args(&["--model", "x", "-n", "3"])))
        );
    }

    #[test]
    fn bench_is_not_in_usage() {
        assert!(
            !parse_command(&args(&["nope"]))
                .unwrap_err()
                .to_string()
                .contains("bench")
        );
    }
}
