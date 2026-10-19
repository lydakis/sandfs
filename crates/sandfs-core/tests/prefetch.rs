//! When the workspace asks a source to prefetch: a walk that reads, and sizes missing
//! for more than one file in a folder. Never a single lookup or read, and never a
//! walk that only lists.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::Duration;

use bytes::Bytes;
use sandfs_core::{Listing, MemorySource, Source, SourceKey, Workspace};

/// A source that reports a directory's sizes only once it was prefetched, as a remote
/// source does once the files are local, and counts calls.
#[derive(Debug, Default)]
struct Remote {
    inner: MemorySource,
    prefetches: Mutex<Vec<(String, bool)>>,
    /// Prefetched directories, and whether everything below them was.
    sized: Mutex<HashSet<(String, bool)>>,
    listings: AtomicUsize,
    reads: AtomicUsize,
    /// How long each prefetch takes.
    delay: Duration,
}

impl Remote {
    fn new(files: &[&str]) -> Self {
        let inner =
            MemorySource::from_files(files.iter().map(|path| (*path, Bytes::from(format!("{path}!"))))).unwrap();
        Remote { inner, ..Default::default() }
    }

    fn prefetches(&self) -> Vec<(String, bool)> {
        let mut calls = self.prefetches.lock().unwrap().clone();
        calls.sort();
        calls
    }
}

impl Source for Remote {
    fn root(&self) -> sandfs_core::Result<SourceKey> {
        self.inner.root()
    }

    fn read_dir(&self, key: &str) -> sandfs_core::Result<Listing> {
        self.listings.fetch_add(1, Ordering::SeqCst);
        let mut listing = self.inner.read_dir(key)?;
        let sized = self.sized.lock().unwrap().iter().any(|(dir, recursive)| {
            dir == key || (*recursive && (dir.is_empty() || key.starts_with(&format!("{dir}/"))))
        });
        if !sized {
            for entry in listing.values_mut() {
                entry.size = None;
            }
        }
        Ok(listing)
    }

    fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.inner.read_file(key)
    }

    fn prefetch(&self, key: &str, recursive: bool) {
        thread::sleep(self.delay);
        self.prefetches.lock().unwrap().push((key.to_owned(), recursive));
        self.sized.lock().unwrap().insert((key.to_owned(), recursive));
    }
}

const TREE: &[&str] = &[
    "a/one.txt",
    "a/b/two.txt",
    "a/b/c/three.txt",
    "a/d/four.txt",
    "e/five.txt",
    "s/x.txt",
    "s/y.txt",
    "s/z.txt",
    "s/w.txt",
];

fn mounted(source: &Arc<Remote>) -> Workspace {
    let mut ws = Workspace::new();
    ws.mount("/data", source.clone()).unwrap();
    ws
}

/// Every file below `dir`, read as found when `read` is set, like `grep -r`.
fn walk(ws: &Workspace, dir: &str, read: bool) -> Vec<String> {
    let mut found = Vec::new();
    for entry in ws.list(dir).unwrap() {
        let path = format!("{dir}/{}", entry.name);
        if entry.kind.is_dir() {
            found.extend(walk(ws, &path, read));
        } else {
            if read {
                ws.read(&path).unwrap();
            }
            found.push(path);
        }
    }
    found
}

fn walked(dirs: &[&str]) -> Vec<(String, bool)> {
    dirs.iter().map(|dir| ((*dir).to_owned(), true)).collect()
}

#[test]
fn lookups_and_reads_never_prefetch() {
    let source = Arc::new(Remote::new(TREE));
    let ws = mounted(&source);
    assert!(ws.exists("/data/a/b/c/three.txt"));
    assert_eq!(ws.read("/data/a/d/four.txt").unwrap(), Bytes::from("a/d/four.txt!"));
    assert_eq!(ws.read_range("/data/a/b/two.txt", 2, 3).unwrap(), Bytes::from("b/t"));
    // Listing one folder, or two that aren't nested, is not a walk.
    ws.list("/data/a/b").unwrap();
    ws.list("/data/e").unwrap();
    ws.read("/data/e/five.txt").unwrap();
    assert_eq!(source.prefetches(), Vec::<(String, bool)>::new());
}

#[test]
fn a_walk_that_only_lists_prefetches_nothing() {
    let source = Arc::new(Remote::new(TREE));
    let ws = mounted(&source);
    assert_eq!(walk(&ws, "/data", false).len(), TREE.len());
    assert_eq!(source.prefetches(), Vec::<(String, bool)>::new());
    assert_eq!(source.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn a_walk_that_reads_prefetches_each_folder_once() {
    let source = Arc::new(Remote::new(TREE));
    let ws = mounted(&source);
    assert_eq!(walk(&ws, "/data/a", true).len(), 4);
    // `a/one.txt` is read before the walk shows itself; from `a/b` on, the walk's start
    // and each folder it reads in are prefetched.
    assert_eq!(source.prefetches(), walked(&["a", "a/b", "a/b/c", "a/d"]));
    // Walking again, or from a fork, asks only for what wasn't asked for yet.
    walk(&ws, "/data/a", true);
    walk(&ws.fork(), "/data", true);
    assert_eq!(source.prefetches(), walked(&["", "a", "a/b", "a/b/c", "a/d", "e", "s"]));
}

#[test]
fn sizes_inside_a_walk_come_from_the_prefetch() {
    let source = Arc::new(Remote::new(TREE));
    let ws = mounted(&source);
    ws.list("/data/a").unwrap();
    ws.list("/data/a/b").unwrap();
    assert_eq!(ws.stat("/data/a/b/two.txt").unwrap().size, "a/b/two.txt!".len() as u64);
    assert_eq!(source.prefetches(), walked(&["a", "a/b"]));
    assert_eq!(source.reads.load(Ordering::SeqCst), 0);
}

#[test]
fn one_missing_size_reads_the_file_and_more_prefetch_the_folder() {
    let source = Arc::new(Remote::new(TREE));
    let ws = mounted(&source);
    let listed = ws.list("/data/s").unwrap();
    assert!(listed.iter().all(|entry| entry.size.is_none()));
    let listings = source.listings.load(Ordering::SeqCst);
    // One file's size says nothing about its neighbours: it is read, as `cat` would.
    assert_eq!(ws.stat("/data/s/x.txt").unwrap().size, 8);
    assert_eq!(ws.stat("/data/s/x.txt").unwrap().size, 8);
    assert_eq!(source.reads.load(Ordering::SeqCst), 1);
    assert_eq!(source.prefetches(), Vec::<(String, bool)>::new());
    // A second file's size (`ls -l`) prefetches the folder and lists it once more.
    assert_eq!(ws.stat("/data/s/y.txt").unwrap().size, 8);
    assert_eq!(ws.stat("/data/s/z.txt").unwrap().size, 8);
    assert_eq!(source.prefetches(), vec![("s".to_owned(), false)]);
    assert_eq!(source.listings.load(Ordering::SeqCst), listings + 1);
    assert_eq!(source.reads.load(Ordering::SeqCst), 1);
    // The new listing replaced the old one, so it shows the sizes too.
    let file = ws.list("/data/s").unwrap().into_iter().find(|entry| entry.name == "w.txt").unwrap();
    assert_eq!(file.size, Some(8));
}

#[test]
fn a_source_that_ignores_the_hint_still_gets_exact_sizes() {
    #[derive(Debug)]
    struct Sizeless(MemorySource);
    impl Source for Sizeless {
        fn root(&self) -> sandfs_core::Result<SourceKey> {
            self.0.root()
        }
        fn read_dir(&self, key: &str) -> sandfs_core::Result<Listing> {
            let mut listing = self.0.read_dir(key)?;
            listing.values_mut().for_each(|entry| entry.size = None);
            Ok(listing)
        }
        fn read_file(&self, key: &str) -> sandfs_core::Result<Bytes> {
            self.0.read_file(key)
        }
    }
    let files = [("a/x.bin", vec![7u8; 9]), ("a/y.bin", vec![7u8; 4])];
    let mut ws = Workspace::new();
    ws.mount("/data", Arc::new(Sizeless(MemorySource::from_files(files).unwrap()))).unwrap();
    assert_eq!(ws.stat("/data/a/x.bin").unwrap().size, 9);
    assert_eq!(ws.stat("/data/a/y.bin").unwrap().size, 4);
    // A base file copied elsewhere has no listing to look in, and is read instead.
    ws.copy("/data/a/y.bin", "/copy.bin", false).unwrap();
    assert_eq!(ws.stat("/copy.bin").unwrap().size, 4);
}

#[test]
fn forks_asking_for_sizes_at_once_prefetch_once() {
    let source = Arc::new(Remote { delay: Duration::from_millis(50), ..Remote::new(TREE) });
    let ws = mounted(&source);
    let barrier = Arc::new(Barrier::new(4));
    let threads: Vec<_> = ["x", "y", "z", "w"]
        .into_iter()
        .map(|name| {
            let (fork, barrier) = (ws.fork(), barrier.clone());
            thread::spawn(move || {
                barrier.wait();
                fork.stat(&format!("/data/s/{name}.txt")).unwrap().size
            })
        })
        .collect();
    for thread in threads {
        assert_eq!(thread.join().unwrap(), 8);
    }
    // One fork was first and read its file; the other three prefetched once.
    assert_eq!(source.prefetches(), vec![("s".to_owned(), false)]);
    assert_eq!(source.reads.load(Ordering::SeqCst), 1);
}
