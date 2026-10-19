//! The mount on macOS: an NFSv3 server on localhost that macOS's own NFS client mounts,
//! so nothing has to be installed and no kernel extension approved.
//!
//! Requests go to the same [`Inner`] as the FUSE mount's, so writes are saved the same
//! way: once a file has gone [`crate::fs::SETTLE`] without a write, or when a save is
//! asked for. Two things differ. `fsync` never reaches an NFS server (writes are
//! answered as already stable), so it saves nothing by itself; instead the server
//! answers save requests on a second localhost port, which `sandfs changes`, `fork`
//! and `unmount` use to have everything written so far saved first. And macOS's NFS
//! client keeps extended attributes in `._name` files, which are kept out of saves.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
#[cfg(nfs)]
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(nfs)]
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use fuser::{Errno, FileAttr, FileType, RenameFlags, TimeOrNow};
use nfsserve::nfs::{
    FSF_CANSETTIME, FSF_HOMOGENEOUS, fattr3, fileid3, filename3, fsinfo3, ftype3, nfspath3, nfsstat3, nfstime3,
    post_op_attr, sattr3, set_mode3, set_mtime, set_size3, specdata3,
};
#[cfg(nfs)]
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
#[cfg(nfs)]
use tokio::io::AsyncWriteExt;

use crate::fs::{FsResult, Inner, MAX_FILE_SIZE, ROOT_INO, Shared};

/// How often the server checks that its mount is still there, in case it went away
/// without telling the server (a forced unmount).
#[cfg(nfs)]
const MOUNT_CHECK: Duration = Duration::from_millis(500);

/// Listing pages kept track of, at most: past this some are forgotten, and a client
/// resuming one of those resumes by the entry's current name.
const PAGE_ENDS: usize = 1024;

/// The [`NFSFileSystem`] handed to the NFS server.
pub struct SandNfs {
    shared: Arc<Shared>,
    /// The name each listing page ended on, by folder and the cookie (inode) the client
    /// resumes from. The entry may be renamed before the next page is asked for, and
    /// the next page must start after where it was, not where it went.
    page_ends: Mutex<BTreeMap<(fileid3, fileid3), String>>,
}

/// The NFS status for an errno.
fn status(err: Errno) -> nfsstat3 {
    match err {
        Errno::ENOENT => nfsstat3::NFS3ERR_NOENT,
        Errno::EEXIST => nfsstat3::NFS3ERR_EXIST,
        Errno::ENOTDIR => nfsstat3::NFS3ERR_NOTDIR,
        Errno::EISDIR => nfsstat3::NFS3ERR_ISDIR,
        Errno::ENOTEMPTY => nfsstat3::NFS3ERR_NOTEMPTY,
        Errno::EINVAL => nfsstat3::NFS3ERR_INVAL,
        Errno::EFBIG => nfsstat3::NFS3ERR_FBIG,
        Errno::EPERM => nfsstat3::NFS3ERR_PERM,
        Errno::EACCES => nfsstat3::NFS3ERR_ACCES,
        Errno::ELOOP => nfsstat3::NFS3ERR_NOTSUPP,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

trait OrStatus<T> {
    fn nfs(self) -> Result<T, nfsstat3>;
}

impl<T> OrStatus<T> for FsResult<T> {
    fn nfs(self) -> Result<T, nfsstat3> {
        self.map_err(status)
    }
}

fn time(time: SystemTime) -> nfstime3 {
    let since = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    nfstime3 { seconds: u32::try_from(since.as_secs()).unwrap_or(u32::MAX), nseconds: since.subsec_nanos() }
}

fn attributes(attr: &FileAttr) -> fattr3 {
    let directory = attr.kind == FileType::Directory;
    fattr3 {
        ftype: if directory { ftype3::NF3DIR } else { ftype3::NF3REG },
        mode: attr.perm.into(),
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        size: attr.size,
        used: attr.blocks * 512,
        rdev: specdata3::default(),
        fsid: 0,
        fileid: attr.ino.0,
        atime: time(attr.atime),
        mtime: time(attr.mtime),
        ctime: time(attr.ctime),
    }
}

fn os_name(name: &filename3) -> &OsStr {
    OsStr::from_bytes(&name.0)
}

impl SandNfs {
    /// Serve `shared` over NFS.
    pub fn new(shared: Arc<Shared>) -> SandNfs {
        SandNfs { shared, page_ends: Mutex::default() }
    }

    /// Run `op` on the filesystem, then wake the saver if it changed something.
    fn change<T>(&self, op: impl FnOnce(&mut Inner) -> FsResult<T>) -> Result<T, nfsstat3> {
        let result = op(&mut self.shared.lock());
        self.shared.changed();
        result.nfs()
    }

    /// Create a file.
    fn create_file(
        &self,
        dirid: fileid3,
        name: &filename3,
        mode: u32,
        exclusive: bool,
        empty: bool,
    ) -> Result<FileAttr, nfsstat3> {
        self.change(|inner| inner.create(dirid, os_name(name), mode, exclusive, empty))
    }
}

#[async_trait]
impl NFSFileSystem for SandNfs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_INO
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let mut inner = self.shared.lock();
        match &filename.0[..] {
            b"." => Ok(dirid),
            b".." => inner.parent_of(dirid).ok_or(nfsstat3::NFS3ERR_NOENT),
            _ => Ok(inner.lookup(dirid, os_name(filename)).nfs()?.ino.0),
        }
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        Ok(attributes(&self.shared.lock().attr(id).nfs()?))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        let mode = match setattr.mode {
            set_mode3::mode(mode) => Some(mode),
            set_mode3::Void => None,
        };
        let size = match setattr.size {
            set_size3::size(size) => Some(size),
            set_size3::Void => None,
        };
        let mtime = match setattr.mtime {
            set_mtime::SET_TO_CLIENT_TIME(t) => {
                Some(TimeOrNow::SpecificTime(UNIX_EPOCH + Duration::new(t.seconds.into(), t.nseconds)))
            }
            set_mtime::SET_TO_SERVER_TIME => Some(TimeOrNow::Now),
            set_mtime::DONT_CHANGE => None,
        };
        // Ownership and access times are not stored, so changing them is a no-op.
        Ok(attributes(&self.change(|inner| inner.setattr(id, mode, size, mtime))?))
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        let inner = self.shared.lock();
        let data = inner.read(id, offset, count).nfs()?;
        let eof = offset + data.len() as u64 >= inner.attr(id).nfs()?.size;
        Ok((data.to_vec(), eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        let attr = self.change(|inner| {
            inner.write(id, offset, data)?;
            inner.attr(id)
        })?;
        Ok(attributes(&attr))
    }

    async fn create(&self, dirid: fileid3, filename: &filename3, attr: sattr3) -> Result<(fileid3, fattr3), nfsstat3> {
        let mode = match attr.mode {
            set_mode3::mode(mode) => mode,
            set_mode3::Void => 0o644,
        };
        let empty = matches!(attr.size, set_size3::size(0));
        let created = self.create_file(dirid, filename, mode, false, empty)?;
        Ok((created.ino.0, attributes(&created)))
    }

    async fn create_exclusive(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        Ok(self.create_file(dirid, filename, 0o644, true, false)?.ino.0)
    }

    async fn mkdir(&self, dirid: fileid3, dirname: &filename3) -> Result<(fileid3, fattr3), nfsstat3> {
        let attr = self.change(|inner| inner.mkdir(dirid, os_name(dirname)))?;
        Ok((attr.ino.0, attributes(&attr)))
    }

    /// REMOVE and RMDIR both land here.
    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        let name = os_name(filename);
        self.change(|inner| {
            if inner.lookup(dirid, name)?.kind == FileType::Directory {
                inner.rmdir(dirid, name)
            } else {
                inner.unlink(dirid, name)
            }
        })
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        self.change(|inner| {
            inner.rename(from_dirid, os_name(from_filename), to_dirid, os_name(to_filename), RenameFlags::empty())
        })
    }

    /// Entries in name order, after the one with id `start_after` (0 for the first page).
    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let mut inner = self.shared.lock();
        let fh = inner.opendir(dirid).nfs()?;
        // Skip `.` and `..`: the client makes those up itself.
        let listing = inner.readdir(fh, 2).map(<[_]>::to_vec);
        inner.releasedir(fh);
        let listing = listing.nfs()?;
        let page_end = self.page_ends.lock().unwrap().get(&(dirid, start_after)).cloned();
        let start = if start_after == 0 {
            0
        } else if let Some(after) = page_end {
            listing.partition_point(|(_, _, name)| *name <= after)
        } else if let Some(position) = listing.iter().position(|(ino, ..)| *ino == start_after) {
            position + 1
        } else {
            // The entry a forgotten page ended on was removed since: resume after its
            // name, so nothing is listed twice or skipped.
            let after = inner.last_name_of(start_after).ok_or(nfsstat3::NFS3ERR_BAD_COOKIE)?;
            listing.partition_point(|(_, _, name)| *name <= after)
        };
        let mut entries: Vec<DirEntry> = Vec::new();
        for (ino, _, name) in listing.iter().skip(start) {
            if entries.len() == max_entries {
                let mut page_ends = self.page_ends.lock().unwrap();
                if page_ends.len() >= PAGE_ENDS {
                    page_ends.pop_first();
                }
                if let Some(last) = entries.last() {
                    page_ends.insert((dirid, last.fileid), String::from_utf8_lossy(&last.name.0).into_owned());
                }
                return Ok(ReadDirResult { entries, end: false });
            }
            // Gone since the listing was made.
            let Ok(attr) = inner.attr(*ino) else { continue };
            inner.hold(*ino);
            entries.push(DirEntry { fileid: *ino, name: name.as_bytes().into(), attr: attributes(&attr) });
        }
        Ok(ReadDirResult { entries, end: true })
    }

    /// The server's defaults, but with sandfs's own file size limit, and no symlinks.
    async fn fsinfo(&self, root_fileid: fileid3) -> Result<fsinfo3, nfsstat3> {
        let attributes = match self.getattr(root_fileid).await {
            Ok(attr) => post_op_attr::attributes(attr),
            Err(_) => post_op_attr::Void,
        };
        Ok(fsinfo3 {
            obj_attributes: attributes,
            rtmax: 1 << 20,
            rtpref: 1 << 20,
            rtmult: 1 << 20,
            wtmax: 1 << 20,
            wtpref: 1 << 20,
            wtmult: 1 << 20,
            dtpref: 1 << 20,
            maxfilesize: MAX_FILE_SIZE as u64,
            time_delta: nfstime3 { seconds: 0, nseconds: 1_000_000 },
            properties: FSF_HOMOGENEOUS | FSF_CANSETTIME,
        })
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
}

/// Serve `shared` over NFS, mount it at `dir` as `name` and keep serving until it is
/// unmounted. `mounted` is called once the mount is up, with the port that answers
/// save requests.
#[cfg(nfs)]
pub fn run(name: &str, dir: &Path, shared: &Arc<Shared>, mounted: impl FnOnce(u16) -> Result<()>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    runtime.block_on(async {
        let mut server = NFSTcpListener::bind("127.0.0.1:0", SandNfs::new(shared.clone()))
            .await
            .context("starting the NFS server")?;
        server.with_export_name(format!("sandfs/{name}"));
        let (mount_events, mut events) = tokio::sync::mpsc::channel(16);
        server.set_mount_listener(mount_events);
        let port = server.get_listen_port();
        tokio::spawn(async move { server.handle_forever().await });
        let saves = tokio::net::TcpListener::bind("127.0.0.1:0").await.context("starting the save listener")?;
        let save_port = saves.local_addr()?.port();
        tokio::spawn(answer_saves(saves, shared.clone()));

        let options = mount_options(port);
        let source = format!("localhost:/sandfs/{name}");
        let point = dir.to_path_buf();
        let status = tokio::task::spawn_blocking(move || {
            std::process::Command::new("/sbin/mount_nfs").args(["-o", &options, &source]).arg(&point).status()
        })
        .await?
        .context("running mount_nfs")?;
        anyhow::ensure!(status.success(), "mount_nfs failed ({status})");
        if let Err(err) = mounted(save_port) {
            // The server stops when this returns, so don't leave a dead mount over `dir`.
            let _ = std::process::Command::new("/sbin/umount").arg("-f").arg(dir).status();
            return Err(err);
        }

        // The client says when it unmounts; checking the mount table as well catches an
        // unmount that never told the server.
        let mut check = tokio::time::interval(MOUNT_CHECK);
        loop {
            tokio::select! {
                event = events.recv() => if event != Some(true) { break },
                _ = check.tick() => if !crate::store::is_mounted(dir) { break },
            }
        }
        Ok(())
    })
}

/// `mount_nfs` options for a server on localhost `port`, which also answers the mount
/// protocol, so no portmapper is needed.
#[cfg(any(nfs, test))]
fn mount_options(port: u16) -> String {
    [
        "vers=3",
        "tcp",
        &format!("port={port},mountport={port}"),
        // Advisory locks stay on this machine, where every program using the mount is.
        "locallocks",
        // Fail instead of hanging forever if this process dies without unmounting.
        "soft",
        "intr",
        // Send each write to the server before it returns, as FUSE does, instead of
        // keeping it in the client's cache, so a save sees every write that returned.
        "sync",
        "rsize=1048576",
        "wsize=1048576",
    ]
    .join(",")
}

/// Answer each connection to `saves` by saving everything written so far, then
/// replying `ok` or the error.
#[cfg(nfs)]
async fn answer_saves(saves: tokio::net::TcpListener, shared: Arc<Shared>) {
    while let Ok((mut stream, _)) = saves.accept().await {
        let shared = shared.clone();
        tokio::spawn(async move {
            let saved = tokio::task::spawn_blocking(move || shared.save(true)).await;
            let reply = match saved {
                Ok(Ok(())) => "ok\n".to_owned(),
                Ok(Err(err)) => format!("{err:#}\n"),
                Err(err) => format!("{err}\n"),
            };
            let _ = stream.write_all(reply.as_bytes()).await;
        });
    }
}

/// Ask the process serving a mount to save, through its save port.
#[cfg(nfs)]
pub fn request_save(port: u16) -> Result<()> {
    use std::io::Read;

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    match reply.trim() {
        "ok" => Ok(()),
        "" => anyhow::bail!("the mount's process stopped before saving"),
        err => anyhow::bail!("{err}"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use sandfs_core::{MemorySource, VPath, Workspace};

    use super::*;
    use crate::fs::is_bookkeeping;
    use crate::persist::Saved;

    fn served() -> (SandNfs, tempfile::TempDir) {
        let source = MemorySource::from_files([("notes.txt", "draft\n"), ("in/a.txt", "a")]).unwrap();
        let mut ws = Workspace::new();
        ws.mount("/mnt", Arc::new(source)).unwrap();
        let root = VPath::parse("/mnt").unwrap();
        let mut inner = Inner::new(ws, &root, BTreeMap::new(), BTreeSet::new(), UNIX_EPOCH, (501, 20));
        inner.keep_removed = false;
        inner.skip_bookkeeping = true;
        let dir = tempfile::tempdir().unwrap();
        (SandNfs::new(Shared::new(inner, Saved::new(dir.path()), root)), dir)
    }

    fn name(s: &str) -> filename3 {
        s.as_bytes().into()
    }

    fn block<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(future)
    }

    #[test]
    fn writes_are_read_back_and_saved() {
        let (fs, _dir) = served();
        block(async {
            let (id, attr) = fs.create(ROOT_INO, &name("new.txt"), sattr3::default()).await.unwrap();
            assert_eq!(attr.size, 0);
            fs.write(id, 0, b"hello ").await.unwrap();
            let attr = fs.write(id, 6, b"world").await.unwrap();
            assert_eq!(attr.size, 11);
            assert_eq!(fs.read(id, 0, 100).await.unwrap(), (b"hello world".to_vec(), true));
            assert_eq!(fs.read(id, 0, 5).await.unwrap(), (b"hello".to_vec(), false));
            assert_eq!(fs.lookup(ROOT_INO, &name("new.txt")).await.unwrap(), id);
        });
        // A save moves the buffered writes into the workspace.
        let snapshot = fs.shared.lock().snapshot(Duration::ZERO);
        assert_eq!(snapshot.ws.read_text("/mnt/new.txt").unwrap(), "hello world");
    }

    #[test]
    fn finder_files_are_served_but_never_saved() {
        let (fs, _dir) = served();
        let snapshot = block(async {
            let (id, _) = fs.create(ROOT_INO, &name("._notes.txt"), sattr3::default()).await.unwrap();
            fs.write(id, 0, b"xattrs").await.unwrap();
            fs.create(ROOT_INO, &name(".DS_Store"), sattr3::default()).await.unwrap();
            let (sub, _) = fs.mkdir(ROOT_INO, &name("sub")).await.unwrap();
            fs.create(sub, &name("._sub"), sattr3::default()).await.unwrap();
            fs.create(sub, &name("kept.txt"), sattr3::default()).await.unwrap();
            assert_eq!(fs.read(id, 0, 100).await.unwrap().0, b"xattrs", "served while mounted");
            fs.shared.lock().snapshot(Duration::ZERO)
        });
        let changed: Vec<String> = snapshot.ws.changes().unwrap().iter().map(|c| c.path.to_string()).collect();
        assert_eq!(changed, ["/mnt/sub/kept.txt"]);
        assert!(is_bookkeeping("._notes.txt") && is_bookkeeping(".DS_Store") && !is_bookkeeping("notes.txt"));
    }

    #[test]
    fn listings_page_by_entry_and_survive_removals() {
        let (fs, _dir) = served();
        block(async {
            for n in ["b", "c", "d"] {
                fs.create(ROOT_INO, &name(n), sattr3::default()).await.unwrap();
            }
            let names = |result: &ReadDirResult| -> Vec<String> {
                result.entries.iter().map(|e| String::from_utf8(e.name.0.clone()).unwrap()).collect()
            };
            let first = fs.readdir(ROOT_INO, 0, 2).await.unwrap();
            assert_eq!((names(&first), first.end), (vec!["b".to_owned(), "c".to_owned()], false));
            let last = first.entries.last().unwrap().fileid;
            // The entry the page ended on goes away before the next page is asked for.
            fs.remove(ROOT_INO, &name("c")).await.unwrap();
            let rest = fs.readdir(ROOT_INO, last, 10).await.unwrap();
            assert_eq!((names(&rest), rest.end), (vec!["d".to_owned(), "in".to_owned(), "notes.txt".to_owned()], true));

            // Or is renamed: the next page still starts after where it was.
            let first = fs.readdir(ROOT_INO, 0, 2).await.unwrap();
            assert_eq!(names(&first), ["b", "d"]);
            fs.rename(ROOT_INO, &name("d"), ROOT_INO, &name("z")).await.unwrap();
            let rest = fs.readdir(ROOT_INO, first.entries.last().unwrap().fileid, 10).await.unwrap();
            assert_eq!(names(&rest), ["in", "notes.txt", "z"]);
        });
    }

    #[test]
    fn removes_renames_and_attributes() {
        let (fs, _dir) = served();
        block(async {
            let notes = fs.lookup(ROOT_INO, &name("notes.txt")).await.unwrap();
            let attr = fs.getattr(notes).await.unwrap();
            assert_eq!((attr.size, attr.mode, attr.uid), (6, 0o644, 501));
            let dir = fs.lookup(ROOT_INO, &name("in")).await.unwrap();
            assert_eq!(fs.lookup(dir, &name("..")).await.unwrap(), ROOT_INO);
            assert_eq!(fs.lookup(dir, &name(".")).await.unwrap(), dir);
            assert!(matches!(fs.getattr(dir).await.unwrap().ftype, ftype3::NF3DIR));

            fs.rename(ROOT_INO, &name("notes.txt"), dir, &name("moved.txt")).await.unwrap();
            assert!(matches!(fs.lookup(ROOT_INO, &name("notes.txt")).await, Err(nfsstat3::NFS3ERR_NOENT)));
            let moved = fs.lookup(dir, &name("moved.txt")).await.unwrap();
            assert_eq!(fs.read(moved, 0, 100).await.unwrap().0, b"draft\n");

            let truncate = sattr3 { size: set_size3::size(2), mode: set_mode3::mode(0o755), ..sattr3::default() };
            let attr = fs.setattr(moved, truncate).await.unwrap();
            assert_eq!((attr.size, attr.mode), (2, 0o755));

            assert!(matches!(fs.remove(ROOT_INO, &name("in")).await, Err(nfsstat3::NFS3ERR_NOTEMPTY)));
            fs.remove(dir, &name("a.txt")).await.unwrap();
            fs.remove(dir, &name("moved.txt")).await.unwrap();
            fs.remove(ROOT_INO, &name("in")).await.unwrap();
            assert!(fs.create_exclusive(ROOT_INO, &name("in")).await.is_ok());
            assert!(matches!(fs.create_exclusive(ROOT_INO, &name("in")).await, Err(nfsstat3::NFS3ERR_EXIST)));
        });
    }

    #[test]
    fn fsinfo_reports_the_real_limits() {
        let (fs, _dir) = served();
        let info = block(fs.fsinfo(ROOT_INO)).unwrap();
        assert_eq!(info.maxfilesize, MAX_FILE_SIZE as u64);
        assert_eq!(info.properties & nfsserve::nfs::FSF_SYMLINK, 0, "symlinks are not supported");
    }

    #[test]
    fn mount_options_name_the_server_port() {
        let options = mount_options(4049);
        assert!(options.contains("port=4049,mountport=4049") && options.contains("vers=3"), "{options}");
        assert!(options.split(',').any(|option| option == "sync"), "{options}");
    }
}
