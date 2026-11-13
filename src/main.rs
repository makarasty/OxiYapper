//! The release build on Windows is a GUI-subsystem program: Explorer starts it with no console
//! window. A terminal still gets the output of `--version`, usage errors and `bench`:
//! [`attach`] connects to the parent's console first. The daemon and the settings window never
//! stay attached: a terminal's Ctrl+C or close must not end the tray app.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod bench;
#[cfg(windows)]
mod console;
#[cfg(windows)]
mod daemon;
mod log;
mod net;
mod pipeline;

use oxi_core::Command;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(windows)]
use console::attach;
/// Nothing to attach to off Windows: there the process always has its terminal.
#[cfg(not(windows))]
fn attach() {}

#[cfg(windows)]
fn run_daemon() {
    if let Err(e) = daemon::run() {
        attach();
        eprintln!("error: {e:#}");
        // Started by Explorer or the Run key the release exe has no console: without the box
        // the app would vanish with no word (t11). The log file has the same line. No IPC
        // listener is alive while it waits (`daemon::run` starts it last), so a second launch
        // starts a daemon of its own instead of being forwarded here.
        #[cfg(not(debug_assertions))]
        oxi_platform::os::tray::error_box(&format!("{} stopped: {e:#}", oxi_core::APP_NAME));
        std::process::exit(1);
    }
}

#[cfg(not(windows))]
fn run_daemon() {
    println!(
        "{} {VERSION}: not supported on this OS yet",
        oxi_core::APP_NAME
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // "Transcribe a file" decodes in a child of the daemon: a damaged file ends only the child.
    if let [arg, path] = args.as_slice()
        && arg == oxi_platform::decode::CHILD_ARG
    {
        std::process::exit(oxi_platform::decode::child_main(
            std::path::Path::new(path),
            oxi_core::audio::SAMPLE_RATE,
        ));
    }
    match oxi_core::parse_command(&args) {
        Ok(Command::Daemon) => run_daemon(),
        Ok(Command::Settings) => {
            if let Err(e) = oxi_settings::run() {
                attach();
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Ok(Command::Version) => {
            attach();
            println!("{} {VERSION}", oxi_core::APP_NAME);
        }
        Ok(Command::Bench(rest)) => {
            attach();
            if let Err(e) = bench::run(&rest) {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        }
        Err(usage) => {
            attach();
            eprintln!("{usage}");
            std::process::exit(2);
        }
    }
}
