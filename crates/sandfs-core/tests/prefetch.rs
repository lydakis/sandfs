//! When the workspace asks a source to prefetch: a folder once two of its files are
//! needed (sizes missing from its listing, or reads in a walk or one after another),
//! and everything below a folder once a walk, or a program reading by path, has needed
//! files in three folders in it without a pause. Never a single lookup or read, never
//! reads with pauses between them, and never a walk that only lists.

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
    /// How long each read takes.
    read_delay: Duration,
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
        thread::sleep(self.read_delay);
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

#[test]
fn lookups_and_reads_in_a_folder_or_two_never_prefetch() {
    let source = Arc::new(Remote::new(TREE));
    let ws = mounted(&source);
    assert!(ws.exists("/data/a/b/c/three.txt"));
    assert_eq!(ws.read("/data/a/d/four.txt").unwrap(), Bytes::from("a/d/four.txt!"));
    // Listing one folder, or two that aren't nested, is not a walk.
    ws.list("/data/a/b").unwrap();
    ws.list("/data/e").unwrap();
    assert_eq!(ws.read_range("/data/a/b/two.txt", 2, 3).unwrap(), Bytes::from("b/t"));
    assert_eq!(source.prefetches(), Vec::<(String, bool)>::new());
}

#[test]
fn reading_by_path_one_file_after_another_prefetches_like_a_walk() {
    let source = Arc::new(Remote::new(TREE));
    let ws = mounted(&source);
    // A build opening the files it needs, without listing a folder: the second file in
    // `s` prefetches `s`, and needing files in a third folder, `a/b`, prefetches
    // everything below the root, which holds all three.
    for path in ["s/x.txt", "s/y.txt", "a/one.txt", "a/b/two.txt"] {
        ws.read(&format!("/data/{path}")).unwrap();
    }
    assert_eq!(source.prefetches(), vec![(String::new(), true), ("s".to_owned(), false)]);
}

#[test]
fn files_slower_to_arrive_than_a_pause_still_come_one_after_another() {
    let source = Arc::new(Remote { read_delay: Duration::from_millis(2100), ..Remote::new(TREE) });
    let ws = mounted(&source);
    // Each read takes longer than a pause, but the next starts as soon as it ends.
    ws.read("/data/s/x.txt").unwrap();
    ws.read("/data/s/y.txt").unwrap();
    assert_eq!(source.prefetches(), vec![("s".to_owned(), false)]);
}

#[test]
fn reading_through_one_part_of_a_tree_prefetches_that_part_too() {
    let source = Arc::new(Remote::new(&["a/1", "b/1", "c/1", "x/p/1", "x/q/1", "x/r/1", "x/s/1"]));
    let ws = mounted(&source);
    // Three folders across the tree prefetch all of it. Reading on through `x`, the
    // last three folders are all in `x`, so `x` is asked for next: a source still
    // fetching the whole tree fetches the part being read first.
    for path in ["a/1", "b/1", "c/1", "x/p/1", "x/q/1", "x/r/1", "x/s/1"] {
        ws.read(&format!("/data/{path}")).unwrap();
    }
    assert_eq!(source.prefetches(), vec![(String::new(), true), ("x".to_owned(), true)]);
}

#[test]
fn a_folder_read_again_counts_as_the_latest() {
    let source = Arc::new(Remote::new(&["x/a/1", "x/a/2", "x/b/1", "x/c/1", "y/1", "z/1"]));
    let ws = mounted(&source);
    // After `y` and `z`, reading in `x/a` again makes it the latest of the last three,
    // so `x/b` and `x/c` are all in `x`.
    for path in ["x/a/1", "y/1", "z/1", "x/a/2", "x/b/1", "x/c/1"] {
        ws.read(&format!("/data/{path}")).unwrap();
    }
    assert_eq!(source.prefetches(), vec![(String::new(), true), ("x".to_owned(), true), ("x/a".to_owned(), false)]);
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
fn a_walk_prefetches_a_folder_it_reads_two_files_in_and_below_three_folders_it_reads() {
    let source = Arc::new(Remote::new(&[
        "w/1.txt",
        "w/2.txt",
        "w/b/1.txt",
        "w/b/2.txt",
        "w/b/c/1.txt",
        "w/d/1.txt",
        "w/d/2.txt",
        "w/e/1.txt",
    ]));
    let ws = mounted(&source);
    assert_eq!(walk(&ws, "/data/w", true).len(), 8);
    // `w`'s own files are read before the walk shows itself, but one after another, so
    // the second prefetches `w`'s files. The walk reads two files in `w/b`, so its
    // files are prefetched. `w/d` is the third folder it reads in, so everything below
    // `w`, which holds all three, is prefetched. Its second file still asks for `w/d`'s
    // own files, since a source may not have fetched all of `w` yet.
    assert_eq!(
        source.prefetches(),
        vec![("w".to_owned(), false), ("w".to_owned(), true), ("w/b".to_owned(), false), ("w/d".to_owned(), false)]
    );
    // Walking again, or from a fork, finds every file read already.
    walk(&ws, "/data/w", true);
    walk(&ws.fork(), "/data", true);
    assert_eq!(source.prefetches().len(), 4);
}

#[test]
fn looking_around_prefetches_no_more_than_a_folder() {
    let source = Arc::new(Remote::new(TREE));
    let ws = mounted(&source);
    // `ls`, `ls a`, `cat a/one.txt`, a lookup and a size further down: one file of each
    // folder, which says nothing about the rest.
    ws.list("/data").unwrap();
    ws.list("/data/a").unwrap();
    ws.read("/data/a/one.txt").unwrap();
    assert!(ws.exists("/data/a/b/c/three.txt"));
    assert_eq!(ws.stat("/data/a/d/four.txt").unwrap().size, "a/d/four.txt!".len() as u64);
    assert_eq!(source.prefetches(), Vec::<(String, bool)>::new());
    // Two sizes in one folder prefetch that folder. Two folders are still not a walk
    // reading through, so nothing below them is prefetched.
    ws.list("/data/s").unwrap();
    ws.stat("/data/s/x.txt").unwrap();
    ws.stat("/data/s/y.txt").unwrap();
    assert_eq!(source.prefetches(), vec![("s".to_owned(), false)]);
    assert_eq!(source.reads.load(Ordering::SeqCst), 3);
}

#[test]
fn looking_around_with_pauses_or_from_forks_prefetches_nothing_below() {
    let source = Arc::new(Remote::new(&["d1/a.txt", "d2/a.txt", "d3/a.txt", "d4/a.txt"]));
    let ws = mounted(&source);
    for dir in ["d1", "d2", "d3", "d4"] {
        ws.list(&format!("/data/{dir}")).unwrap();
    }
    ws.list("/data").unwrap();
    // Three forks, each listing and reading in its own folder: each is one look.
    for dir in ["d1", "d2", "d3"] {
        let fork = ws.fork();
        fork.list("/data").unwrap();
        fork.list(&format!("/data/{dir}")).unwrap();
        fork.read(&format!("/data/{dir}/a.txt")).unwrap();
    }
    assert_eq!(source.prefetches(), Vec::<(String, bool)>::new());
    // One workspace reading in three folders, but pausing between looks, and another
    // reading two files of one folder by path, with a pause between them.
    let ws = mounted(&source);
    ws.list("/data").unwrap();
    for dir in ["d1", "d2", "d4"] {
        ws.list(&format!("/data/{dir}")).unwrap();
    }
    ws.read_range("/data/d1/a.txt", 0, 1).unwrap();
    ws.read_range("/data/d2/a.txt", 0, 1).unwrap();
    let other = Arc::new(Remote::new(TREE));
    let by_path = mounted(&other);
    by_path.read("/data/s/x.txt").unwrap();
    let pieces = Arc::new(Remote::new(TREE));
    let in_pieces = mounted(&pieces);
    in_pieces.read_range("/data/s/x.txt", 0, 1).unwrap();
    thread::sleep(Duration::from_millis(2100));
    ws.read_range("/data/d4/a.txt", 0, 1).unwrap();
    by_path.read("/data/s/y.txt").unwrap();
    assert_eq!(source.prefetches(), Vec::<(String, bool)>::new());
    assert_eq!(other.prefetches(), Vec::<(String, bool)>::new());
    // The read after the pause starts a new look, so the next file right after it is
    // the second.
    by_path.read("/data/s/z.txt").unwrap();
    assert_eq!(other.prefetches(), vec![("s".to_owned(), false)]);
    // That holds when the read after the pause is of a file read before, in pieces.
    in_pieces.read_range("/data/s/x.txt", 1, 1).unwrap();
    assert_eq!(pieces.prefetches(), Vec::<(String, bool)>::new());
    in_pieces.read_range("/data/s/y.txt", 0, 1).unwrap();
    assert_eq!(pieces.prefetches(), vec![("s".to_owned(), false)]);
}

#[test]
fn files_read_in_earlier_looks_count_again_in_a_new_one() {
    let source = Arc::new(Remote::new(TREE));
    let ws = mounted(&source);
    // Pieces of two files, each in its own look, then of both one after another.
    ws.read_range("/data/s/x.txt", 0, 1).unwrap();
    thread::sleep(Duration::from_millis(2100));
    ws.read_range("/data/s/y.txt", 0, 1).unwrap();
    thread::sleep(Duration::from_millis(2100));
    assert_eq!(source.prefetches(), Vec::<(String, bool)>::new());
    ws.read_range("/data/s/x.txt", 1, 1).unwrap();
    ws.read_range("/data/s/y.txt", 1, 1).unwrap();
    assert_eq!(source.prefetches(), vec![("s".to_owned(), false)]);
}

#[test]
fn a_file_read_in_pieces_counts_once() {
    let source = Arc::new(Remote::new(TREE));
    let ws = mounted(&source);
    ws.list("/data").unwrap();
    ws.list("/data/s").unwrap();
    // Ranged reads don't keep the whole file, so each piece is fetched again, but it is
    // still one file and says nothing about its folder.
    for offset in 0..4 {
        ws.read_range("/data/s/x.txt", offset, 1).unwrap();
    }
    assert_eq!(source.prefetches(), Vec::<(String, bool)>::new());
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
