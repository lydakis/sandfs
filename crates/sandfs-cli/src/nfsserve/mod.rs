//! An NFSv3 server: [nfsserve] 0.11.0 (BSD-3-Clause, see `LICENSE` here), copied in
//! so it can be changed here first. Changes from upstream:
//!
//! - [`transaction_tracker`] drops old transactions once a second instead of on every
//!   call, whose cost grew with the number of calls a minute and took most of the
//!   server's time under load: a cold Rust build on the mount took 65 s with it and
//!   14 s without.
//! - The generation number is a `OnceLock` instead of a `static mut`, as sandfs
//!   forbids unsafe code.
//! - Left out: `fs_util` and the `filetime` conversion, which sandfs doesn't use.
//!
//! [nfsserve]: https://github.com/huggingface/nfsserve

#![allow(missing_docs, dead_code, clippy::all)]

mod context;
mod rpc;
mod rpcwire;
mod write_counter;
pub mod xdr;

mod mount;
mod mount_handlers;

mod portmap;
mod portmap_handlers;

pub mod nfs;
mod nfs_handlers;

pub mod tcp;
mod transaction_tracker;
pub mod vfs;
