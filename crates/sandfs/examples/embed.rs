//! A program with commands of its own that uses sandfs: `embed copy FOLDER DIR`
//! mounts a fork of FOLDER at DIR, `embed changes NAME` lists what it changed, and
//! `embed drop NAME` deletes it. sandfs runs this program again as `embed sandfs ...`
//! to serve the mount.

use anyhow::{Result, bail};

fn main() -> Result<()> {
    sandfs::handle_relaunch(&["sandfs"]);
    let home = sandfs::Home::locate()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["copy", folder, dir] => {
            let mounted = home.mount(folder, dir.as_ref(), &Default::default())?;
            println!("{} at {}", mounted.name, mounted.dir.display());
        }
        ["changes", name] => {
            for change in home.changes(name, &Default::default())?.changes {
                println!("{:?} {}", change.kind, change.path);
            }
        }
        ["drop", name] => home.remove(name, true)?,
        _ => bail!("usage: embed copy FOLDER DIR | embed changes NAME | embed drop NAME"),
    }
    Ok(())
}
