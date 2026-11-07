//! Embeds the app icon and the application manifest (`assets/oxi.rc`) into the Windows
//! executable. `assets/oxi.ico` is drawn by `oxi_platform::draw::logo`; regenerate it with
//! `cargo test -p oxi-platform --test draw regen_icons -- --ignored`.

fn main() {
    for file in ["assets/oxi.rc", "assets/oxi.ico", "assets/oxi.manifest"] {
        println!("cargo:rerun-if-changed={file}");
    }
    if let Err(e) =
        embed_resource::compile("assets/oxi.rc", embed_resource::NONE).manifest_required()
    {
        panic!("icon and manifest not embedded: {e}");
    }
}
