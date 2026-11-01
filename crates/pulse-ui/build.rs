//! Compiles the Slint UI. The cosmic-dark widget style is used because its
//! teal accent matches Pulse's palette (std-widget palettes are read-only).

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("cosmic-dark".into());
    if let Err(e) = slint_build::compile_with_config("ui/main.slint", config) {
        panic!("Slint compilation failed: {e}");
    }
}
