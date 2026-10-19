//! Sets `cfg(fuse)` where the FUSE mount is built: the `mount` feature on a Unix target
//! other than macOS, which has no FUSE of its own.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(fuse)");
    let unix = std::env::var_os("CARGO_CFG_UNIX").is_some();
    let macos = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos");
    if unix && !macos && std::env::var_os("CARGO_FEATURE_MOUNT").is_some() {
        println!("cargo::rustc-cfg=fuse");
    }
}
