//! Embeds the app icon and the application manifest (`assets/oxi.rc`) into the Windows
//! executable, and delay-loads `vulkan-1.dll` (below). `assets/oxi.ico` is drawn by
//! `oxi_platform::draw::logo`; regenerate it with
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
    // vulkan-1.dll comes with GPU drivers, not with us: delay-load it so the exe starts without
    // it; src/vulkan_loader.rs then turns the missing loader into CPU instead of a crash.
    if std::env::var_os("CARGO_FEATURE_VULKAN").is_some()
        && std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc")
    {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:vulkan-1.dll");
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
}
