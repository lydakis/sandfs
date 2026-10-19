//! Sets `cfg(fuse)` where the FUSE mount is built: the `mount` feature on a Unix target.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(fuse)");
    let unix = std::env::var_os("CARGO_CFG_UNIX").is_some();
    if unix && std::env::var_os("CARGO_FEATURE_MOUNT").is_some() {
        println!("cargo::rustc-cfg=fuse");
    }
}
