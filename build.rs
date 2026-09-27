//! Tell cargo to rebuild when the embedded JavaScript changes.
//!
//! `include_str!` dependency tracking did not fire reliably here, so edits to
//! the shim or the vendored bundles silently produced a stale binary — which
//! is a genuinely nasty way to lose an afternoon, because every test then
//! reports the behaviour of code you already changed.
use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    for dir in ["src/js", "src/js/vendor"] {
        println!("cargo:rerun-if-changed={dir}");
        if let Ok(entries) = std::fs::read_dir(Path::new(dir)) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "js").unwrap_or(false) {
                    println!("cargo:rerun-if-changed={}", path.display());
                }
            }
        }
    }
}
