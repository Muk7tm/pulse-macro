//! Compiles the Slint UI. Pulse draws its controls from its own black-and-
//! white theme; the cosmic-dark widget style only supplies the scroll views,
//! whose scroll bars are neutral grey (std-widget palettes are read-only).

fn main() {
    let config = slint_build::CompilerConfiguration::new().with_style("cosmic-dark".into());
    if let Err(e) = slint_build::compile_with_config("ui/main.slint", config) {
        panic!("Slint compilation failed: {e}");
    }
}
