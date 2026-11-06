mod bench;
mod net;

use oxi_core::Command;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match oxi_core::parse_command(&args) {
        Ok(Command::Daemon) => {
            println!(
                "{} {VERSION}: daemon not implemented yet",
                oxi_core::APP_NAME
            );
            println!("{}", oxi_whisper::system_info());
        }
        Ok(Command::Settings) => println!("settings UI not implemented yet"),
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
