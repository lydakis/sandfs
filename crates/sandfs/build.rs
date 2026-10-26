//! Sets the cfgs for what this build can mount: `fuse` (Linux) or `nfs` (macOS), and
//! `mount` for either. Both need the `mount` feature. Elsewhere sandfs could mount
//! but not find or unmount its mounts, so it doesn't.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(fuse)");
    println!("cargo::rustc-check-cfg=cfg(nfs)");
    println!("cargo::rustc-check-cfg=cfg(mount)");
    if std::env::var_os("CARGO_FEATURE_MOUNT").is_none() {
        return;
    }
    match std::env::var("CARGO_CFG_TARGET_OS").as_deref() {
        Ok("linux") => println!("cargo::rustc-cfg=fuse"),
        Ok("macos") => println!("cargo::rustc-cfg=nfs"),
        _ => return,
    }
    println!("cargo::rustc-cfg=mount");
}
