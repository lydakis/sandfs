//! A source with a fixed delay per call, like an API or an object store, read lazily
//! by sandfs or copied up front.
//!
//! These workloads go through the Rust API only. The baseline fetches every listing
//! and file before reading, which is what a tool that imports data up front pays.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use bytes::Bytes;
use sandfs_core::{Listing, MemorySource, Source, SourceKey, Workspace};

use crate::data::Data;

/// Wraps a source, sleeping before every call and counting the calls.
#[derive(Debug)]
pub struct Slow {
    inner: MemorySource,
    latency: Duration,
    calls: AtomicUsize,
}

impl Slow {
    fn wait(&self) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        std::thread::sleep(self.latency);
    }

    /// Calls made so far.
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl Source for Slow {
    fn root(&self) -> sandfs_core::Result<SourceKey> {
        self.inner.root()
    }

    fn read_dir(&self, key: &str) -> sandfs_core::Result<Listing> {
        self.wait();
        self.inner.read_dir(key)
    }

    fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
        self.wait();
        self.inner.read_file(key)
    }
}

/// The result of one lazy workload: time and source calls for each approach.
pub struct LazyResult {
    pub name: &'static str,
    pub what: String,
    pub lazy: Vec<(Duration, usize)>,
    pub eager: Vec<(Duration, usize)>,
}

struct Setup {
    files: Vec<(String, Bytes)>,
    latency: Duration,
}

impl Setup {
    fn source(&self) -> Result<Arc<Slow>> {
        let inner = MemorySource::from_files(self.files.iter().map(|(path, data)| (path.as_str(), data.clone())))?;
        Ok(Arc::new(Slow { inner, latency: self.latency, calls: AtomicUsize::new(0) }))
    }

    fn paths(&self, n: usize) -> Vec<String> {
        let step = (self.files.len() / n.max(1)).max(1);
        self.files.iter().step_by(step).take(n).map(|(path, _)| format!("/src/{path}")).collect()
    }
}

/// Every file of `source`, fetched by walking it: the up-front import.
fn fetch_all(source: &Slow) -> Result<HashMap<String, Bytes>> {
    let mut files = HashMap::new();
    let mut stack = vec![(String::new(), source.root()?)];
    while let Some((dir, key)) = stack.pop() {
        for (name, entry) in source.read_dir(&key)? {
            let path = format!("{dir}/{name}");
            if entry.kind.is_dir() {
                stack.push((path, entry.key));
            } else {
                files.insert(format!("/src{path}"), source.read_file(&entry.key)?);
            }
        }
    }
    Ok(files)
}

fn lazy_read(setup: &Setup, paths: &[String]) -> Result<(Duration, usize)> {
    let source = setup.source()?;
    let start = Instant::now();
    let mut ws = Workspace::new();
    ws.mount("/src", source.clone())?;
    for path in paths {
        ws.read(path)?;
    }
    Ok((start.elapsed(), source.calls()))
}

fn eager_read(setup: &Setup, paths: &[String]) -> Result<(Duration, usize)> {
    let source = setup.source()?;
    let start = Instant::now();
    let files = fetch_all(&source)?;
    for path in paths {
        ensure!(files.contains_key(path), "{path} missing from the copy");
    }
    Ok((start.elapsed(), source.calls()))
}

const THREADS: usize = 8;

fn lazy_concurrent(setup: &Setup, paths: &[String]) -> Result<(Duration, usize)> {
    let source = setup.source()?;
    let start = Instant::now();
    let mut ws = Workspace::new();
    ws.mount("/src", source.clone())?;
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let fork = ws.fork();
                scope.spawn(move || paths.iter().try_for_each(|path| fork.read(path).map(drop)))
            })
            .collect();
        handles.into_iter().try_for_each(|handle| handle.join().expect("reader thread"))
    })?;
    Ok((start.elapsed(), source.calls()))
}

fn eager_concurrent(setup: &Setup, paths: &[String]) -> Result<(Duration, usize)> {
    let source = setup.source()?;
    let start = Instant::now();
    let files = Arc::new(fetch_all(&source)?);
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            let files = files.clone();
            scope.spawn(move || paths.iter().all(|path| files.contains_key(path)));
        }
    });
    Ok((start.elapsed(), source.calls()))
}

/// Run the lazy-source workloads over the first `scale.lazy_files` small files.
pub fn run(data: &Data, latency: Duration, iters: usize) -> Result<Vec<LazyResult>> {
    let n = data.scale.lazy_files.min(data.small.len());
    let files = data.small[..n]
        .iter()
        .map(|path| Ok((path.clone(), Bytes::from(std::fs::read(data.root().join(path))?))))
        .collect::<Result<Vec<_>>>()?;
    let setup = Setup { files, latency };
    let one_percent = setup.paths((n / 100).max(1));
    let all = setup.paths(n);
    let tenth = setup.paths((n / 10).max(1));
    let us = latency.as_micros();

    type Run = fn(&Setup, &[String]) -> Result<(Duration, usize)>;
    let cases: [(&str, String, &[String], Run, Run); 3] = [
        (
            "lazy/read-1%",
            format!("mount {n} files behind {us} µs per call, read {}", one_percent.len()),
            &one_percent,
            lazy_read,
            eager_read,
        ),
        ("lazy/read-all", format!("same, read all {n}"), &all, lazy_read, eager_read),
        (
            "lazy/forks",
            format!("{THREADS} forks on threads each read the same {} files", tenth.len()),
            &tenth,
            lazy_concurrent,
            eager_concurrent,
        ),
    ];
    let mut results = Vec::new();
    for (name, what, paths, lazy, eager) in cases {
        eprintln!("  {name}");
        let mut result = LazyResult { name, what, lazy: Vec::new(), eager: Vec::new() };
        for _ in 0..iters {
            result.lazy.push(lazy(&setup, paths)?);
            result.eager.push(eager(&setup, paths)?);
        }
        results.push(result);
    }
    Ok(results)
}
