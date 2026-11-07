//! The release build on Windows is a GUI-subsystem program: Explorer starts it with no console
//! window. A terminal still gets the output of `--version`, usage errors and `bench`:
//! [`console::attach`] connects to the parent's console first.
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
fn run_daemon() {
    if let Err(e) = daemon::run() {
        eprintln!("error: {e:#}");
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
    #[cfg(windows)]
    console::attach();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match oxi_core::parse_command(&args) {
        Ok(Command::Daemon) => run_daemon(),
        Ok(Command::Settings) => {
            if let Err(e) = oxi_settings::run() {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        Ok(Command::Version) => println!("{} {VERSION}", oxi_core::APP_NAME),
        Ok(Command::Bench(rest)) => {
            if let Err(e) = bench::run(&rest) {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        }
        Err(usage) => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    }
}
