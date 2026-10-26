//! Build-time detection of a compiled web UI.
//!
//! `web/dist` is produced by the frontend toolchain and is not part of the Rust
//! build. When an index page is present, the whole directory is embedded in the
//! `smw` binary; without it the server falls back to a placeholder page so a
//! plain `cargo build` still succeeds.

use std::path::Path;

fn main() {
    println!("cargo:rustc-check-cfg=cfg(web_dist_present)");
    println!("cargo:rerun-if-changed=web/dist");

    if Path::new("web/dist/index.html").exists() {
        println!("cargo:rustc-cfg=web_dist_present");
    }
}
