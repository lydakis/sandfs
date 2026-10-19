//! Sets `cfg(fuse)` where the FUSE mount is built: the `mount` feature on Linux, the one
//! system where sandfs can also find and unmount its mounts.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(fuse)");
    let linux = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux");
    if linux && std::env::var_os("CARGO_FEATURE_MOUNT").is_some() {
        println!("cargo::rustc-cfg=fuse");
    }
}
