fn main() {
    // The tests find elements by accessible label, which needs Slint's debug info; release
    // builds stay without it.
    let debug = std::env::var("PROFILE").is_ok_and(|p| p == "debug");
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        .with_debug_info(debug);
    slint_build::compile_with_config("ui/app.slint", config).expect("compile ui/app.slint");

    // The design-system gallery: its own module in OUT_DIR, included only by the `gallery`
    // example and test, so the app's generated code does not change.
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let manifest =
        std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets it"));
    let gallery = slint_build::CompilerConfiguration::new().with_style("fluent".into());
    let deps = slint_build::compile_with_output_path(
        manifest.join("ui/gallery.slint"),
        out.join("gallery.rs"),
        gallery,
    )
    .expect("compile ui/gallery.slint");
    for dep in deps {
        println!("cargo:rerun-if-changed={}", dep.display());
    }
}
