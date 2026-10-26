//! Two attempts at tidying a folder, each in its own copy, and the better one written
//! back. Everything happens under a temporary folder, with a sandfs home of its own,
//! so this leaves nothing behind: `cargo run -p sandfs --example attempts`.
//!
//! Mounting needs FUSE 3 on Linux (`fusermount3`); macOS needs nothing.

use std::fs;

use anyhow::Result;
use sandfs::{ChangesOptions, Home};

fn main() -> Result<()> {
    // sandfs serves mounts by running this program again; those runs end here.
    sandfs::handle_relaunch(&[]);

    let scratch = tempfile::tempdir()?;
    let notes = scratch.path().join("notes");
    fs::create_dir(&notes)?;
    fs::write(notes.join("monday.txt"), "call the plumber\n")?;
    fs::write(notes.join("tuesday.txt"), "buy stamps\n")?;
    fs::write(notes.join("scratch.txt"), "asdf\n")?;

    let home = Home::at(scratch.path().join("home"))?;
    let base = home.mount(notes.to_str().unwrap(), &scratch.path().join("base"), &Default::default())?;

    // Two copies of the same folder, made without copying it.
    let mut attempts = Vec::new();
    for name in ["merge", "index"] {
        let dir = scratch.path().join(name);
        attempts.push(home.fork(&base.name, Some(&dir), &Default::default())?);
    }

    // Each copy is a real folder, so any program can work in it. Here: one merges
    // the notes into one file, the other only drops the scratch file and adds an index.
    let merge = attempts[0].dir.clone().unwrap();
    let all = fs::read_to_string(merge.join("monday.txt"))? + &fs::read_to_string(merge.join("tuesday.txt"))?;
    fs::write(merge.join("week.txt"), all)?;
    for file in ["monday.txt", "tuesday.txt", "scratch.txt"] {
        fs::remove_file(merge.join(file))?;
    }
    let index = attempts[1].dir.clone().unwrap();
    fs::remove_file(index.join("scratch.txt"))?;
    fs::write(index.join("index.txt"), "monday.txt\ntuesday.txt\n")?;

    for attempt in &attempts {
        println!("{}:", attempt.name);
        for change in home.changes(&attempt.name, &Default::default())?.changes {
            println!("  {:?} {}", change.kind, change.path);
        }
    }

    // The folder underneath is untouched until we choose: write the second attempt's
    // changes into it.
    assert!(notes.join("scratch.txt").exists());
    let keep = ChangesOptions { copy_to: Some(notes.clone()), ..Default::default() };
    home.changes(&attempts[1].name, &keep)?;
    let mut kept: Vec<_> = fs::read_dir(&notes)?.map(|entry| entry.map(|e| e.file_name())).collect::<Result<_, _>>()?;
    kept.sort();
    println!("notes now: {kept:?}");

    for attempt in &attempts {
        home.remove(&attempt.name, true)?;
    }
    home.remove(&base.name, true)?;
    Ok(())
}
