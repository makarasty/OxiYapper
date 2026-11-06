fn main() {
    // The tests find elements by accessible label, which needs Slint's debug info; release
    // builds stay without it.
    let debug = std::env::var("PROFILE").is_ok_and(|p| p == "debug");
    let config = slint_build::CompilerConfiguration::new()
        .with_style("fluent".into())
        .with_debug_info(debug);
    slint_build::compile_with_config("ui/app.slint", config).expect("compile ui/app.slint");
}
