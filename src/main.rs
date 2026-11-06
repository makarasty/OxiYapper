use oxi_core::Command;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    match oxi_core::parse_command(std::env::args().nth(1).as_deref()) {
        Ok(Command::Daemon) => {
            println!("oxi-yapper {VERSION}: daemon not implemented yet");
            println!("{}", oxi_whisper::system_info());
        }
        Ok(Command::Settings) => println!("settings UI not implemented yet"),
        Ok(Command::Version) => println!("oxi-yapper {VERSION}"),
        Err(usage) => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    }
}
