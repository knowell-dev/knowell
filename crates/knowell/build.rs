//! Embeds Cargo's exact build target for the stateless update handshake.
fn main() {
    if let Ok(target) = std::env::var("TARGET") {
        println!("cargo:rustc-env=KNOWELL_BUILD_TARGET={target}");
    }
}
