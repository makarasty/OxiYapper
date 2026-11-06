const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    match std::env::args().nth(1).as_deref() {
        None => {
            println!("oxi-yapper {VERSION}: daemon not implemented yet");
            println!("{}", whisper_rs::print_system_info());
        }
        Some("settings") => println!("settings UI not implemented yet"),
        Some("--version") => println!("oxi-yapper {VERSION}"),
        Some(other) => {
            eprintln!("unknown argument: {other}\nusage: oxi-yapper [settings|--version]");
            std::process::exit(2);
        }
    }
}
