//! The data set every target serves: generated once per run, deterministically.
//!
//! It is not code. The files are arbitrary bytes in four shapes that stress different
//! parts of a filesystem: many small files, one wide directory, one deep chain of
//! directories and one large file.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Result;
use tempfile::TempDir;

/// How big the data set is and how many operations a workload does.
#[derive(Clone, Copy, Debug)]
pub struct Scale {
    /// Directories under `small/`.
    pub dirs: usize,
    /// Files in each of those directories.
    pub files_per_dir: usize,
    /// Files in `wide/`.
    pub wide: usize,
    /// Directories in the `deep/` chain.
    pub depth: usize,
    /// Size of `large/blob.bin` in MiB.
    pub large_mib: usize,
    /// Writes, deletes and ranged reads per workload.
    pub ops: usize,
    /// Timed runs per workload and target.
    pub iters: usize,
    /// Files the lazy-source workloads mount.
    pub lazy_files: usize,
}

impl Scale {
    /// The scale the published numbers use.
    pub const FULL: Scale = Scale {
        dirs: 100,
        files_per_dir: 100,
        wide: 10_000,
        depth: 64,
        large_mib: 256,
        ops: 1000,
        iters: 5,
        lazy_files: 1000,
    };
    /// A few seconds per target, for trying changes out.
    pub const QUICK: Scale = Scale {
        dirs: 20,
        files_per_dir: 50,
        wide: 2000,
        depth: 32,
        large_mib: 32,
        ops: 200,
        iters: 3,
        lazy_files: 200,
    };
    /// Just enough to exercise every workload, for tests.
    pub const TINY: Scale =
        Scale { dirs: 3, files_per_dir: 5, wide: 20, depth: 4, large_mib: 1, ops: 5, iters: 1, lazy_files: 10 };

    /// Number of files under `small/`.
    pub fn small_files(&self) -> usize {
        self.dirs * self.files_per_dir
    }
}

/// The generated tree and the paths workloads use, relative to its root.
pub struct Data {
    dir: TempDir,
    pub scale: Scale,
    /// Every file under `small/`, in creation order.
    pub small: Vec<String>,
    /// Every directory under `small/`, plus `small` itself.
    pub small_dirs: Vec<String>,
    /// Total bytes under `small/`.
    pub small_bytes: u64,
    /// The file at the bottom of `deep/`.
    pub deep_file: String,
    /// The large file and its size.
    pub large_file: String,
    pub large_size: u64,
}

impl Data {
    /// Generate the tree under a new temporary directory inside `parent`.
    pub fn generate(scale: Scale, parent: &Path) -> Result<Data> {
        let dir = tempfile::Builder::new().prefix("sandfs-bench-data-").tempdir_in(parent)?;
        let root = dir.path();
        let mut rng = Rng(0x5eed_f00d);

        let mut small = Vec::new();
        let mut small_dirs = vec!["small".to_owned()];
        let mut small_bytes = 0;
        for d in 0..scale.dirs {
            let rel_dir = format!("small/d{d:03}");
            fs::create_dir_all(root.join(&rel_dir))?;
            for f in 0..scale.files_per_dir {
                let rel = format!("{rel_dir}/f{f:03}.bin");
                // 1 to 16 KiB, the size of a typical note, config or thumbnail.
                let size = 1024 + (rng.next() % (15 * 1024)) as usize;
                fs::write(root.join(&rel), rng.bytes(size))?;
                small_bytes += size as u64;
                small.push(rel);
            }
            small_dirs.push(rel_dir);
        }

        fs::create_dir_all(root.join("wide"))?;
        for e in 0..scale.wide {
            fs::write(root.join(format!("wide/e{e:05}")), rng.bytes(64))?;
        }

        let mut deep = PathBuf::from("deep");
        for level in 0..scale.depth {
            deep.push(format!("level{level:02}"));
        }
        fs::create_dir_all(root.join(&deep))?;
        deep.push("leaf.bin");
        fs::write(root.join(&deep), rng.bytes(256))?;

        fs::create_dir_all(root.join("large"))?;
        let large_size = (scale.large_mib as u64) << 20;
        let mut large = fs::File::create(root.join("large/blob.bin"))?;
        let chunk = rng.bytes(1 << 20);
        for _ in 0..scale.large_mib {
            large.write_all(&chunk)?;
        }
        large.sync_all()?;

        Ok(Data {
            dir,
            scale,
            small,
            small_dirs,
            small_bytes,
            deep_file: deep.to_str().expect("ASCII path").to_owned(),
            large_file: "large/blob.bin".to_owned(),
            large_size,
        })
    }

    /// Root of the generated tree on the host.
    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    /// `n` small files spread evenly over the tree.
    pub fn spread(&self, n: usize) -> Vec<&str> {
        let step = (self.small.len() / n.max(1)).max(1);
        self.small.iter().step_by(step).take(n).map(String::as_str).collect()
    }
}

/// A small deterministic generator (xorshift64*), so every run uses the same bytes and
/// offsets without a dependency.
pub struct Rng(pub u64);

impl Rng {
    /// The next pseudo-random number.
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// `len` pseudo-random bytes.
    pub fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            out.extend_from_slice(&self.next().to_le_bytes());
        }
        out.truncate(len);
        out
    }
}
