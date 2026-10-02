//! Chooses which panel build to embed.
//!
//! When `panel/dist/index.html` exists (CI release jobs download it before
//! `cargo build`), the crate is compiled with `--cfg knowell_panel_dist` and
//! embeds `panel/dist`. Otherwise it embeds `panel-placeholder/`, a single
//! page explaining how to build the panel, so Rust-only builds never fail.
//!
//! Cargo cannot watch a directory that does not exist yet without re-running
//! this script (and recompiling the crate) on every build, so a panel built
//! *after* this crate was compiled is picked up only on the next rebuild of
//! the crate (`cargo clean -p knowell-server`, or touch this file).

use std::path::PathBuf;

fn main() {
    println!("cargo::rustc-check-cfg=cfg(knowell_panel_dist)");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=panel-placeholder");

    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_default();
    let dist = manifest_dir
        .join("..")
        .join("..")
        .join("panel")
        .join("dist");
    if dist.join("index.html").is_file() {
        println!("cargo::rustc-cfg=knowell_panel_dist");
        println!("cargo::rerun-if-changed=../../panel/dist");
    } else {
        println!(
            "cargo::warning=panel/dist was not found; embedding the placeholder panel page (build the panel with `npm --prefix panel run build`, then rebuild this crate)"
        );
    }
}
