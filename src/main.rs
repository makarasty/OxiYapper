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
#[cfg(all(windows, target_env = "msvc"))]
mod vulkan_loader;

use oxi_core::Command;

use oxi_core::VERSION;

#[cfg(windows)]
use console::attach;
/// Nothing to attach to off Windows: there the process always has its terminal.
#[cfg(not(windows))]
fn attach() {}

#[cfg(windows)]
fn run_daemon(launch: Launch) {
    if let Err(e) = daemon::run(launch) {
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

/// How the daemon was started; nothing to say off Windows.
#[cfg(windows)]
use daemon::Launch;
#[cfg(not(windows))]
type Launch = ();

#[cfg(not(windows))]
fn run_daemon(_: Launch) {
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
    // The watchdog of an update, started by the daemon that installed it from the replaced exe.
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some(daemon::WATCH_ARG) {
        return daemon::watch_update(&args[1..]);
    }
    // A daemon the previous one started after installing an update: it takes the pipe once the
    // old process has ended.
    #[cfg(windows)]
    if args.first().map(String::as_str) == Some(daemon::AFTER_UPDATE_ARG) {
        daemon::wait_for_parent();
        return run_daemon(Launch {
            updated: true,
            open_settings: args.iter().any(|a| a == daemon::OPEN_SETTINGS_ARG),
        });
    }
    match oxi_core::parse_command(&args) {
        Ok(Command::Daemon) => run_daemon(Launch::default()),
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
