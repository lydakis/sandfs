//! The mount on macOS: an NFSv3 server on localhost that macOS's own NFS client mounts,
//! so nothing has to be installed and no kernel extension approved.
//!
//! Requests go to the same [`Inner`] as the FUSE mount's, so writes are saved the same
//! way: once a file has gone [`crate::fs::SETTLE`] without a write, or when a save is
//! asked for. Two things differ. `fsync` never reaches an NFS server (writes are
//! answered as already stable), so it saves nothing by itself; instead the server
//! answers save requests on a second localhost port, which `sandfs changes`, `fork`
//! and `unmount` use to have everything written so far saved first. And macOS's NFS
//! client keeps extended attributes in `._name` files, which are kept out of saves and
//! out of listings, so git, cargo and the like never see them.
//!
//! The NFS server itself is [`crate::nfsserve`], a copy of the nfsserve crate.
//!
//! Any local user can connect to a port on localhost, so the server only answers its
//! owner. The port the mount uses is a relay that passes on only calls whose
//! credentials name the user who mounted it (or root), as a FUSE mount only lets its
//! owner in. The relay answers the mount request itself, handing out the root's file
//! handle once, to a request naming a secret export path. The server behind it refuses
//! every mount request and every file handle without another secret, so a program
//! connecting to it directly gets nothing. Credentials can be made up, but not those
//! secrets.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
#[cfg(nfs)]
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::nfsserve::nfs::{
    FSF_CANSETTIME, FSF_HOMOGENEOUS, FSF_SYMLINK, fattr3, fileid3, filename3, fsinfo3, ftype3, nfs_fh3, nfspath3,
    nfsstat3, nfstime3, post_op_attr, sattr3, set_mode3, set_mtime, set_size3, specdata3,
};
#[cfg(nfs)]
use crate::nfsserve::tcp::{NFSTcp, NFSTcpListener};
use crate::nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
#[cfg(nfs)]
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use fuser::{Errno, FileAttr, FileType, RenameFlags, TimeOrNow};
#[cfg(nfs)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::fs::{FsResult, Inner, MAX_FILE_SIZE, ROOT_INO, Shared};

/// How often the server checks that its mount is still there.
#[cfg(nfs)]
const MOUNT_CHECK: Duration = Duration::from_millis(500);

/// Listed names kept track of, at most: past this the oldest are forgotten, and a
/// client resuming after one of those resumes by the entry's current name.
const LISTED_NAMES: usize = 16384;

/// The largest RPC message relayed to the server: a write of `wsize` and its header.
#[cfg(nfs)]
const MAX_MESSAGE: usize = 2 << 20;

/// Names handed out in listings, forgetting the oldest past [`LISTED_NAMES`].
#[derive(Default)]
struct Listed {
    names: HashMap<(fileid3, fileid3), String>,
    order: VecDeque<(fileid3, fileid3)>,
}

impl Listed {
    /// Note that `key` was listed as `name`. An entry listed again under a new name
    /// keeps whichever name sorts first, since an older page may still be resumed from
    /// it: resuming from the earlier name can list some entries twice, but never skips one.
    fn insert(&mut self, key: (fileid3, fileid3), name: String) {
        match self.names.entry(key) {
            Entry::Occupied(mut listed) => {
                if name < *listed.get() {
                    listed.insert(name);
                }
            }
            Entry::Vacant(listed) => {
                listed.insert(name);
                self.order.push_back(key);
                if self.order.len() > LISTED_NAMES
                    && let Some(oldest) = self.order.pop_front()
                {
                    self.names.remove(&oldest);
                }
            }
        }
    }
}

/// The [`NFSFileSystem`] handed to the NFS server.
pub struct SandNfs {
    shared: Arc<Shared>,
    /// The names entries were listed under, by folder and inode, the cookie a client
    /// resumes from. The entry may be renamed before the next page is asked for, and
    /// the next page must start after where it was, not where it went.
    listed: Mutex<Listed>,
    /// Starts every file handle, so handles can't be made up.
    secret: [u8; 16],
}

/// 16 bytes no other process can guess.
fn random() -> std::io::Result<[u8; 16]> {
    use std::io::Read;

    let mut bytes = [0; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// The NFS status for an errno.
fn status(err: Errno) -> nfsstat3 {
    match err {
        Errno::ENOENT => nfsstat3::NFS3ERR_NOENT,
        Errno::EEXIST => nfsstat3::NFS3ERR_EXIST,
        Errno::ENOTDIR => nfsstat3::NFS3ERR_NOTDIR,
        Errno::EISDIR => nfsstat3::NFS3ERR_ISDIR,
        Errno::ENOTEMPTY => nfsstat3::NFS3ERR_NOTEMPTY,
        Errno::EROFS => nfsstat3::NFS3ERR_ROFS,
        Errno::EINVAL => nfsstat3::NFS3ERR_INVAL,
        Errno::EFBIG => nfsstat3::NFS3ERR_FBIG,
        Errno::ENOSPC => nfsstat3::NFS3ERR_NOSPC,
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
    fattr3 {
        ftype: match attr.kind {
            FileType::Directory => ftype3::NF3DIR,
            FileType::Symlink => ftype3::NF3LNK,
            _ => ftype3::NF3REG,
        },
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
    pub fn new(shared: Arc<Shared>) -> std::io::Result<SandNfs> {
        Ok(SandNfs { shared, listed: Mutex::default(), secret: random()? })
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

    fn id_to_fh(&self, id: fileid3) -> nfs_fh3 {
        nfs_fh3 { data: [&self.secret[..], &id.to_le_bytes()].concat() }
    }

    fn fh_to_id(&self, fh: &nfs_fh3) -> Result<fileid3, nfsstat3> {
        match fh.data.split_at_checked(self.secret.len()) {
            Some((secret, id)) if secret == self.secret => {
                Ok(u64::from_le_bytes(id.try_into().map_err(|_| nfsstat3::NFS3ERR_BADHANDLE)?))
            }
            _ => Err(nfsstat3::NFS3ERR_BADHANDLE),
        }
    }

    /// Asked for by a mount request, which the relay answers itself: one that reaches
    /// the server came from somewhere else, and gets nothing.
    async fn path_to_id(&self, _path: &[u8]) -> Result<fileid3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_ACCES)
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
            // A WRITE reply says the whole request was written, so a short write (the
            // disk filled up partway) is reported as the full disk it is.
            if (inner.write(id, offset, data)? as usize) < data.len() {
                return Err(Errno::ENOSPC);
            }
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
            let attr = inner.lookup(dirid, name)?;
            if attr.kind == FileType::Directory {
                inner.clear_appledouble(attr.ino.0)?;
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
        let sizeless = inner.sizeless(fh);
        inner.releasedir(fh);
        let (mut listing, sizeless) = (listing.nfs()?, sizeless.nfs()?);
        listing.retain(|(_, kind, name)| !inner.is_added_appledouble(dirid, name, *kind));
        let listed_as = self.listed.lock().unwrap().names.get(&(dirid, start_after)).cloned();
        let start = if start_after == 0 {
            0
        } else if let Some(after) = listed_as {
            listing.partition_point(|(_, _, name)| *name <= after)
        } else if let Some(position) = listing.iter().position(|(ino, ..)| *ino == start_after) {
            position + 1
        } else {
            // The entry a forgotten page ended on was removed since: resume after its
            // name, so nothing is listed twice or skipped.
            let after = inner.last_name_of(start_after).ok_or(nfsstat3::NFS3ERR_BAD_COOKIE)?;
            listing.partition_point(|(_, _, name)| *name <= after)
        };
        let (mut entries, mut names) = (Vec::new(), Vec::new());
        let mut end = true;
        for (ino, _, name) in listing.iter().skip(start) {
            if entries.len() == max_entries {
                end = false;
                break;
            }
            // A file's size can mean reading it, so one the listing had no size for comes
            // without attributes, which the client then asks for if it needs them (`ls -l`,
            // a read). A walk that needs only names (`find`, `git status`) reads nothing.
            let attr = if sizeless.contains(ino) {
                None
            } else {
                // Gone since the listing was made.
                let Ok(attr) = inner.attr(*ino) else { continue };
                Some(attributes(&attr))
            };
            inner.hold(*ino);
            entries.push(DirEntry { fileid: *ino, name: name.as_bytes().into(), attr });
            names.push(name.clone());
        }
        // The server may send only some of these and end the page early, so the client
        // can resume after any of them.
        let mut listed = self.listed.lock().unwrap();
        for (entry, name) in entries.iter().zip(names) {
            listed.insert((dirid, entry.fileid), name);
        }
        Ok(ReadDirResult { entries, end })
    }

    /// The server's defaults, but with sandfs's own file size limit.
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
            properties: FSF_SYMLINK | FSF_HOMOGENEOUS | FSF_CANSETTIME,
        })
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        let attr = self.change(|inner| inner.symlink(dirid, os_name(linkname), OsStr::from_bytes(&symlink.0)))?;
        Ok((attr.ino.0, attributes(&attr)))
    }

    async fn readlink(&self, id: fileid3) -> Result<nfspath3, nfsstat3> {
        let target = self.shared.lock().readlink(id).nfs()?;
        Ok(target.into_bytes().into())
    }
}

/// Serve `shared` over NFS, mount it at `dir` as `name` and keep serving until it is
/// unmounted. `mounted` is called once the mount is up, with the port that answers
/// save requests and the secret they must carry.
#[cfg(nfs)]
pub fn run(
    name: &str,
    dir: &Path,
    shared: &Arc<Shared>,
    mounted: impl FnOnce(u16, String) -> Result<()>,
) -> Result<()> {
    // Each mount has a runtime of its own, so everything it started stops with it. Calls
    // take turns on the workspace, so a couple of threads keep up.
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    runtime.block_on(async {
        let fs = SandNfs::new(shared.clone())?;
        let root = fs.id_to_fh(ROOT_INO).data;
        let server = NFSTcpListener::bind("127.0.0.1:0", fs).await.context("starting the NFS server")?;
        let export = format!("/sandfs/{name}/{}", hex(&random()?));
        let server_port = server.get_listen_port();
        tokio::spawn(async move { server.handle_forever().await });
        let front = tokio::net::TcpListener::bind("127.0.0.1:0").await.context("starting the NFS listener")?;
        let port = front.local_addr()?.port();
        let users = vec![nix::unistd::getuid().as_raw(), 0];
        let gate = Gate { users, export: export.clone(), root, mounted: AtomicBool::new(false) };
        tokio::spawn(guard(front, server_port, Arc::new(gate)));
        let saves = tokio::net::TcpListener::bind("127.0.0.1:0").await.context("starting the save listener")?;
        let save_port = saves.local_addr()?.port();
        let save_secret = hex(&random()?);
        tokio::spawn(answer_saves(saves, shared.clone(), save_secret.clone()));

        let options = mount_options(port);
        let source = format!("localhost:{export}");
        let point = dir.to_path_buf();
        let status = tokio::task::spawn_blocking(move || {
            std::process::Command::new("/sbin/mount_nfs").args(["-o", &options, &source]).arg(&point).status()
        })
        .await?
        .context("running mount_nfs")?;
        anyhow::ensure!(status.success(), "mount_nfs failed ({status})");
        if let Err(err) = mounted(save_port, save_secret) {
            // The server stops when this returns, so don't leave a dead mount over `dir`.
            let _ = std::process::Command::new("/sbin/umount").arg("-f").arg(dir).status();
            return Err(err);
        }

        // The relay keeps the client's unmount call from the server, since anyone could
        // send one, so the mount table says when the mount is gone (or replaced).
        let mut check = tokio::time::interval(MOUNT_CHECK);
        let ours = format!("sandfs:{name}");
        while crate::host::mount_source(dir).is_some_and(|source| source == ours) {
            check.tick().await;
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

/// What the relay decides each call by.
#[cfg(nfs)]
struct Gate {
    /// The users whose calls pass.
    users: Vec<u32>,
    /// The secret export path, which only the `mount_nfs` this process runs names.
    export: String,
    /// The root's file handle, which the relay hands out itself.
    root: Vec<u8>,
    /// Whether it was.
    mounted: AtomicBool,
}

/// Relay each connection to `front` to the NFS server on `server_port`, passing on
/// only the calls [`call_allowed`] lets through for the gate's users, refusing the
/// rest, and answering mount and unmount requests itself.
#[cfg(nfs)]
async fn guard(front: tokio::net::TcpListener, server_port: u16, gate: Arc<Gate>) {
    while let Ok((client, _)) = front.accept().await {
        let gate = gate.clone();
        tokio::spawn(async move { relay(client, server_port, &gate).await });
    }
}

#[cfg(nfs)]
async fn relay(client: tokio::net::TcpStream, server_port: u16, gate: &Gate) -> std::io::Result<()> {
    let server = tokio::net::TcpStream::connect(("127.0.0.1", server_port)).await?;
    client.set_nodelay(true)?;
    server.set_nodelay(true)?;
    let (mut from_client, to_client) = client.into_split();
    let (mut from_server, mut to_server) = server.into_split();
    // Replies and refusals both go to the client, a whole message at a time.
    let to_client = Arc::new(tokio::sync::Mutex::new(to_client));
    // The calls asking what access a caller has, whose replies need correcting.
    let access_calls = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let replies = {
        let to_client = to_client.clone();
        let access_calls = access_calls.clone();
        tokio::spawn(async move {
            while let Ok(Some((mut message, mut reply))) = read_message(&mut from_server).await {
                let xid = reply.get(..4).map(|xid| u32::from_be_bytes(xid.try_into().unwrap()));
                if xid.is_some_and(|xid| access_calls.lock().unwrap().remove(&xid)) {
                    limit_access(&mut reply);
                    message = [&(0x8000_0000 | reply.len() as u32).to_be_bytes()[..], &reply].concat();
                }
                if to_client.lock().await.write_all(&message).await.is_err() {
                    break;
                }
            }
        })
    };
    let result = async {
        while let Some((message, call)) = read_message(&mut from_client).await? {
            if is_unmount_call(&call) {
                to_client.lock().await.write_all(&unmounted(&call)).await?;
            } else if !call_allowed(&call, &gate.users) {
                to_client.lock().await.write_all(&refusal(&call)).await?;
            } else if is_mount_call(&call) {
                let reply = mount_reply(&call, &gate.export, &gate.root, &gate.mounted);
                to_client.lock().await.write_all(&reply).await?;
            } else {
                if is_access_call(&call) {
                    access_calls.lock().unwrap().insert(u32::from_be_bytes(call[..4].try_into().unwrap()));
                }
                to_server.write_all(&message).await?;
            }
        }
        Ok(())
    }
    .await;
    replies.abort();
    result
}

/// Read one RPC message: its bytes as they came, record marks included, and its body.
#[cfg(nfs)]
async fn read_message(stream: &mut tokio::net::tcp::OwnedReadHalf) -> std::io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let (mut message, mut body) = (Vec::new(), Vec::new());
    loop {
        let mut mark = [0; 4];
        match stream.read_exact(&mut mark).await {
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof && message.is_empty() => return Ok(None),
            result => result?,
        };
        let mark_value = u32::from_be_bytes(mark);
        let len = (mark_value & 0x7fff_ffff) as usize;
        if body.len() + len > MAX_MESSAGE {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "RPC message too large"));
        }
        let start = body.len();
        body.resize(start + len, 0);
        stream.read_exact(&mut body[start..]).await?;
        message.extend_from_slice(&mark);
        message.extend_from_slice(&body[start..]);
        if mark_value & 0x8000_0000 != 0 {
            return Ok(Some((message, body)));
        }
    }
}

/// Whether an RPC call may reach the server: a ping (procedure 0) always, anything
/// else only with Unix credentials naming one of `users`. The mount protocol's
/// listings (DUMP and EXPORT) never pass: EXPORT would name the secret export path,
/// and Unix credentials are only what the caller claims, so the secret is what keeps
/// other users out.
fn call_allowed(call: &[u8], users: &[u32]) -> bool {
    const MOUNT: u32 = 100005;
    let word = |at: usize| call.get(at..at + 4).map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()));
    // xid, CALL, RPC version, program, version, procedure, then the credentials:
    // flavor, length, and for AUTH_UNIX a stamp, the machine name and the uid.
    if word(4) != Some(0) {
        return false;
    }
    if word(12) == Some(MOUNT) && matches!(word(20), Some(2 | 5)) {
        return false;
    }
    if word(20) == Some(0) {
        return true;
    }
    let (Some(1), Some(name_len)) = (word(24), word(36)) else { return false };
    let uid = word(40 + (name_len as usize).next_multiple_of(4));
    uid.is_some_and(|uid| users.contains(&uid))
}

/// Whether `call` is a mount-protocol UMNT or UMNTALL call. The server stops serving
/// when it gets one, whoever sent it, so the relay answers these itself; the server
/// sees the mount go from the mount table instead.
fn is_unmount_call(call: &[u8]) -> bool {
    let word = |at: usize| call.get(at..at + 4).map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()));
    word(4) == Some(0) && word(12) == Some(100005) && matches!(word(20), Some(3 | 4))
}

/// Whether `call` is a mount-protocol MNT call.
fn is_mount_call(call: &[u8]) -> bool {
    let word = |at: usize| call.get(at..at + 4).map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()));
    word(4) == Some(0) && word(12) == Some(100005) && word(20) == Some(1)
}

/// The reply to a mount request: the `root` file handle the first time the secret
/// `export` path is asked for, and a refusal for anything else.
fn mount_reply(call: &[u8], export: &str, root: &[u8], mounted: &AtomicBool) -> Vec<u8> {
    const MNT3_OK: u32 = 0;
    const MNT3ERR_ACCES: u32 = 13;
    const AUTH_UNIX: u32 = 1;
    let word = |at: usize| call.get(at..at + 4).map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()));
    // After the header come the credentials and the verifier, each a flavor, a length
    // and that many bytes (padded to 4), then the path asked for.
    let path = || {
        let verifier = 32 + (word(28)? as usize).next_multiple_of(4);
        let args = verifier + 8 + (word(verifier + 4)? as usize).next_multiple_of(4);
        call.get(args + 4..args + 4 + word(args)? as usize)
    };
    let granted = path().is_some_and(|path| path.strip_suffix(b"/").unwrap_or(path) == export.as_bytes())
        && !mounted.swap(true, Ordering::SeqCst);
    // xid, REPLY, MSG_ACCEPTED, an empty AUTH_NULL verifier and SUCCESS, then the result.
    let xid = call.get(..4).unwrap_or(&[0; 4]);
    let mut reply = [xid, &[1u32, 0, 0, 0, 0].map(u32::to_be_bytes).concat()].concat();
    if granted {
        reply.extend(MNT3_OK.to_be_bytes());
        reply.extend((root.len() as u32).to_be_bytes());
        reply.extend(root);
        reply.resize(reply.len().next_multiple_of(4), 0);
        // One accepted flavor: Unix credentials, which the relay checks.
        reply.extend([1, AUTH_UNIX].map(u32::to_be_bytes).concat());
    } else {
        reply.extend(MNT3ERR_ACCES.to_be_bytes());
    }
    [&(0x8000_0000 | reply.len() as u32).to_be_bytes()[..], &reply].concat()
}

/// The reply to an unmount call: accepted, and done.
fn unmounted(call: &[u8]) -> Vec<u8> {
    let xid = call.get(..4).unwrap_or(&[0; 4]);
    // REPLY, MSG_ACCEPTED, an empty AUTH_NULL verifier, SUCCESS, and no result.
    let words = [1u32, 0, 0, 0, 0].map(u32::to_be_bytes).concat();
    [&(0x8000_0000u32 | 24).to_be_bytes()[..], xid, &words].concat()
}

/// Whether `call` is an NFS ACCESS call (program 100003, procedure 4).
fn is_access_call(call: &[u8]) -> bool {
    call.get(12..16) == Some(&100003u32.to_be_bytes()) && call.get(20..24) == Some(&4u32.to_be_bytes())
}

/// Take execute access out of an ACCESS reply for a file with no execute bit. The
/// server grants whatever is asked, and macOS's client trusts it even to run a
/// program, where Linux checks the bits itself; either way, only files marked
/// executable run.
fn limit_access(reply: &mut [u8]) {
    const EXECUTE: u32 = 0x20;
    const ATTRIBUTES: usize = 84;
    let word =
        |reply: &[u8], at: usize| reply.get(at..at + 4).map(|bytes| u32::from_be_bytes(bytes.try_into().unwrap()));
    // xid, REPLY, MSG_ACCEPTED, the verifier (flavor, length, body), SUCCESS, NFS3_OK,
    // then whether attributes follow, the attributes (type, mode, ...) and the access.
    let Some(verifier) = word(reply, 16) else { return };
    let at = 20 + (verifier as usize).next_multiple_of(4);
    let header = [word(reply, 4), word(reply, 8), word(reply, at), word(reply, at + 4), word(reply, at + 8)];
    if header != [Some(1), Some(0), Some(0), Some(0), Some(1)] {
        return;
    }
    let access_at = at + 12 + ATTRIBUTES;
    if let (Some(1), Some(mode), Some(access)) = (word(reply, at + 12), word(reply, at + 16), word(reply, access_at))
        && mode & 0o111 == 0
    {
        reply[access_at..access_at + 4].copy_from_slice(&(access & !EXECUTE).to_be_bytes());
    }
}

/// The record-marked reply refusing `call`: denied, its credentials too weak.
fn refusal(call: &[u8]) -> Vec<u8> {
    let xid = call.get(..4).unwrap_or(&[0; 4]);
    // REPLY, MSG_DENIED, AUTH_ERROR, AUTH_TOOWEAK.
    let words = [1u32, 1, 1, 5].map(u32::to_be_bytes).concat();
    [&(0x8000_0000u32 | 20).to_be_bytes()[..], xid, &words].concat()
}

/// Answer each connection to `saves` by saving everything written so far, then
/// Answer each connection to `saves` that sends `secret` by saving everything written
/// so far, then replying `ok` or the error. Other local users can reach the port too,
/// so a connection without the secret is dropped unanswered.
#[cfg(nfs)]
async fn answer_saves(saves: tokio::net::TcpListener, shared: Arc<Shared>, secret: String) {
    while let Ok((mut stream, _)) = saves.accept().await {
        let (shared, secret) = (shared.clone(), secret.clone());
        tokio::spawn(async move {
            let mut asked = vec![0; secret.len()];
            let read = tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut asked)).await;
            if !matches!(read, Ok(Ok(_))) || !same_bytes(&asked, secret.as_bytes()) {
                return;
            }
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
pub fn request_save(port: u16, secret: &str) -> Result<()> {
    use std::io::{Read, Write};

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port))?;
    stream.write_all(secret.as_bytes())?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    match reply.trim() {
        "ok" => Ok(()),
        "" => anyhow::bail!("the mount's process stopped before saving"),
        err => anyhow::bail!("{err}"),
    }
}

/// `bytes` in hexadecimal.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Whether `a` and `b` are equal, taking as long whichever byte differs.
fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use sandfs_core::{MemorySource, VPath, Workspace};

    use super::*;
    use crate::fs::is_bookkeeping;
    use crate::persist::Saved;

    fn served() -> (SandNfs, tempfile::TempDir) {
        served_from(&[("notes.txt", "draft\n"), ("in/a.txt", "a")])
    }

    fn served_from(files: &[(&'static str, &'static str)]) -> (SandNfs, tempfile::TempDir) {
        serve(Arc::new(MemorySource::from_files(files.iter().copied()).unwrap()))
    }

    fn serve(source: Arc<dyn sandfs_core::Source>) -> (SandNfs, tempfile::TempDir) {
        let mut ws = Workspace::new();
        ws.mount("/mnt", source).unwrap();
        let root = VPath::parse("/mnt").unwrap();
        let mut inner = Inner::new(ws, &root, BTreeMap::new(), BTreeSet::new(), UNIX_EPOCH, (501, 20));
        inner.keep_removed = false;
        inner.skip_bookkeeping = true;
        let dir = tempfile::tempdir().unwrap();
        (SandNfs::new(Shared::new(inner, Saved::new(dir.path()), root)).unwrap(), dir)
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
            // A folder with such a name is someone's: macOS only makes files.
            fs.mkdir(ROOT_INO, &name("._cache")).await.unwrap();
            assert_eq!(fs.read(id, 0, 100).await.unwrap().0, b"xattrs", "served while mounted");
            fs.shared.lock().snapshot(Duration::ZERO)
        });
        let changed: Vec<String> = snapshot.ws.changes().unwrap().iter().map(|c| c.path.to_string()).collect();
        assert_eq!(changed, ["/mnt/sub/kept.txt"]);
        assert!(snapshot.dirs.iter().any(|dir| dir.ends_with("/._cache")), "{:?}", snapshot.dirs);
        assert!(is_bookkeeping("._notes.txt") && is_bookkeeping(".DS_Store") && !is_bookkeeping("notes.txt"));
    }

    #[test]
    fn listings_leave_out_attributes_that_mean_reading_a_file() {
        /// A source that lists no sizes and counts the files it reads.
        #[derive(Debug)]
        struct Sizeless(MemorySource, std::sync::atomic::AtomicUsize);
        impl sandfs_core::Source for Sizeless {
            fn root(&self) -> sandfs_core::Result<sandfs_core::SourceKey> {
                self.0.root()
            }
            fn read_dir(&self, key: &str) -> sandfs_core::Result<sandfs_core::Listing> {
                let mut listing = self.0.read_dir(key)?;
                listing.values_mut().for_each(|entry| entry.size = None);
                Ok(listing)
            }
            fn read_file(&self, key: &str) -> sandfs_core::Result<bytes::Bytes> {
                self.1.fetch_add(1, Ordering::SeqCst);
                self.0.read_file(key)
            }
        }
        let source = Arc::new(Sizeless(
            MemorySource::from_files([("README.md", "hello\n"), ("src/lib.rs", "")]).unwrap(),
            Default::default(),
        ));
        let (fs, _dir) = serve(source.clone());
        block(async {
            let attrs = |result: ReadDirResult| -> Vec<(String, Option<u64>)> {
                let entries = result.entries.into_iter();
                entries.map(|e| (String::from_utf8(e.name.0).unwrap(), e.attr.map(|a| a.size))).collect()
            };
            let listed = attrs(fs.readdir(ROOT_INO, 0, 100).await.unwrap());
            assert_eq!(listed, [("README.md".to_owned(), None), ("src".to_owned(), Some(0))]);
            assert_eq!(source.1.load(Ordering::SeqCst), 0, "listing read no file");
            // The client asks when it needs them, and from then on listings have them.
            let readme = fs.lookup(ROOT_INO, &name("README.md")).await.unwrap();
            assert_eq!(fs.getattr(readme).await.unwrap().size, 6);
            let listed = attrs(fs.readdir(ROOT_INO, 0, 100).await.unwrap());
            assert_eq!(listed[0], ("README.md".to_owned(), Some(6)));
            assert_eq!(source.1.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn appledouble_files_stay_out_of_listings() {
        let (fs, _dir) = served_from(&[("notes.txt", "draft\n"), ("._data", "theirs")]);
        block(async {
            let names = |result: ReadDirResult| -> Vec<String> {
                result.entries.iter().map(|e| String::from_utf8(e.name.0.clone()).unwrap()).collect()
            };
            let (id, _) = fs.create(ROOT_INO, &name("._notes.txt"), sattr3::default()).await.unwrap();
            fs.write(id, 0, b"xattrs").await.unwrap();
            let (sub, _) = fs.mkdir(ROOT_INO, &name("sub")).await.unwrap();
            fs.create(sub, &name("._kept.txt"), sattr3::default()).await.unwrap();
            fs.mkdir(ROOT_INO, &name("._cache")).await.unwrap();
            // Only the ones macOS made are left out, and they still open by name.
            assert_eq!(names(fs.readdir(ROOT_INO, 0, 100).await.unwrap()), ["._cache", "._data", "notes.txt", "sub"]);
            assert_eq!(fs.lookup(ROOT_INO, &name("._notes.txt")).await.unwrap(), id);
            assert!(names(fs.readdir(sub, 0, 100).await.unwrap()).is_empty());

            // A folder that lists empty can be removed, and one that doesn't can't.
            fs.create(sub, &name("kept.txt"), sattr3::default()).await.unwrap();
            assert!(matches!(fs.remove(ROOT_INO, &name("sub")).await, Err(nfsstat3::NFS3ERR_NOTEMPTY)));
            fs.remove(sub, &name("kept.txt")).await.unwrap();
            fs.remove(ROOT_INO, &name("sub")).await.unwrap();
            assert!(fs.lookup(ROOT_INO, &name("sub")).await.is_err());
        });
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

            // The server may send fewer entries than a page holds: the client resumes
            // after the last one it got, which may also have been renamed.
            let first = fs.readdir(ROOT_INO, 0, 3).await.unwrap();
            assert_eq!(names(&first), ["b", "in", "notes.txt"]);
            fs.rename(ROOT_INO, &name("b"), ROOT_INO, &name("y")).await.unwrap();
            let rest = fs.readdir(ROOT_INO, first.entries[0].fileid, 10).await.unwrap();
            assert_eq!(names(&rest), ["in", "notes.txt", "y", "z"]);

            // A renamed entry listed again under its new name before an older page is
            // resumed from it: the older page still resumes after the old name.
            let first = fs.readdir(ROOT_INO, 0, 1).await.unwrap();
            assert_eq!(names(&first), ["in"]);
            fs.rename(ROOT_INO, &name("in"), ROOT_INO, &name("zz")).await.unwrap();
            fs.readdir(ROOT_INO, 0, 10).await.unwrap();
            let rest = fs.readdir(ROOT_INO, first.entries[0].fileid, 10).await.unwrap();
            assert_eq!(names(&rest), ["notes.txt", "y", "z", "zz"]);
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
    fn removed_files_are_forgotten() {
        let (fs, _dir) = served();
        block(async {
            fs.readdir(ROOT_INO, 0, 1000).await.unwrap();
            let known = fs.shared.lock().node_count();
            for n in 0..100 {
                let (id, _) = fs.create(ROOT_INO, &name(&format!("tmp{n}")), sattr3::default()).await.unwrap();
                fs.write(id, 0, b"scratch").await.unwrap();
                fs.readdir(ROOT_INO, 0, 1000).await.unwrap();
                fs.remove(ROOT_INO, &name(&format!("tmp{n}"))).await.unwrap();
                assert!(fs.getattr(id).await.is_err(), "its handle is gone");
            }
            assert_eq!(fs.shared.lock().node_count(), known, "nothing kept for removed files");
        });
    }

    #[test]
    fn fsinfo_reports_the_real_limits() {
        let (fs, _dir) = served();
        let info = block(fs.fsinfo(ROOT_INO)).unwrap();
        assert_eq!(info.maxfilesize, MAX_FILE_SIZE as u64);
        assert_ne!(info.properties & crate::nfsserve::nfs::FSF_SYMLINK, 0, "symlinks are supported");
    }

    #[test]
    fn symlinks_are_created_read_and_removed() {
        let (fs, _dir) = served();
        block(async {
            let target = "in/a.txt".as_bytes().to_vec().into();
            let (id, attr) = fs.symlink(ROOT_INO, &name("link"), &target, &sattr3::default()).await.unwrap();
            assert!(matches!(attr.ftype, ftype3::NF3LNK));
            assert_eq!((attr.size, attr.mode), (8, 0o777));
            assert_eq!(fs.readlink(id).await.unwrap().0, b"in/a.txt");
            assert_eq!(fs.lookup(ROOT_INO, &name("link")).await.unwrap(), id);
            assert!(matches!(
                fs.symlink(ROOT_INO, &name("link"), &target, &sattr3::default()).await,
                Err(nfsstat3::NFS3ERR_EXIST)
            ));
            let notes = fs.lookup(ROOT_INO, &name("notes.txt")).await.unwrap();
            assert!(matches!(fs.readlink(notes).await, Err(nfsstat3::NFS3ERR_INVAL)));
        });
        let snapshot = fs.shared.lock().snapshot(Duration::ZERO);
        assert_eq!(snapshot.ws.read_link("/mnt/link").unwrap(), "in/a.txt");
        block(async { fs.remove(ROOT_INO, &name("link")).await.unwrap() });
        assert!(!fs.shared.lock().snapshot(Duration::ZERO).ws.exists("/mnt/link"));
    }

    #[test]
    fn handles_carry_the_secret_and_the_server_never_mounts() {
        let (fs, _dir) = served();
        let other = served().0;
        let fh = fs.id_to_fh(7);
        assert!(matches!(fs.fh_to_id(&fh), Ok(7)));
        assert!(matches!(other.fh_to_id(&fh), Err(nfsstat3::NFS3ERR_BADHANDLE)), "another server's handle");
        let mut forged = fh.data[16..].to_vec();
        forged.splice(0..0, 0u64.to_le_bytes());
        assert!(matches!(fs.fh_to_id(&nfs_fh3 { data: forged }), Err(nfsstat3::NFS3ERR_BADHANDLE)));
        assert!(matches!(fs.fh_to_id(&nfs_fh3 { data: fh.data[..20].to_vec() }), Err(nfsstat3::NFS3ERR_BADHANDLE)));
        block(async {
            assert!(matches!(fs.path_to_id(b"/").await, Err(nfsstat3::NFS3ERR_ACCES)), "mounts go through the relay");
        });
    }

    /// An RPC call to `procedure` with AUTH_UNIX credentials for `uid`.
    fn call(procedure: u32, machine: &str, uid: u32) -> Vec<u8> {
        mount_call(100003, procedure, machine, uid)
    }

    /// An RPC call to `procedure` of `program` with AUTH_UNIX credentials for `uid`.
    fn mount_call(program: u32, procedure: u32, machine: &str, uid: u32) -> Vec<u8> {
        let mut name = machine.as_bytes().to_vec();
        name.resize(machine.len().next_multiple_of(4), 0);
        let creds = [&[0, 0, 0, 0][..], &(machine.len() as u32).to_be_bytes(), &name, &uid.to_be_bytes(), &[0; 8]];
        let creds = creds.concat();
        let head = [42u32, 0, 2, program, 3, procedure, 1, creds.len() as u32].map(u32::to_be_bytes).concat();
        [head, creds, vec![0; 8]].concat()
    }

    #[test]
    fn secrets_are_hex_and_compared_whole() {
        assert_eq!(hex(&[0x0f, 0xa0]), "0fa0");
        assert!(same_bytes(b"0fa0", b"0fa0"));
        assert!(!same_bytes(b"0fa0", b"0fa1") && !same_bytes(b"0fa", b"0fa0"));
    }

    #[test]
    fn only_calls_from_the_owner_reach_the_server() {
        assert!(call_allowed(&call(4, "mac", 501), &[501, 0]));
        assert!(call_allowed(&call(4, "longer-name", 0), &[501, 0]), "root");
        assert!(!call_allowed(&call(4, "mac", 502), &[501, 0]), "another user");
        assert!(!call_allowed(&call(4, "mac", 501)[..40], &[501, 0]), "cut short");
        assert!(call_allowed(&call(0, "mac", 502), &[501, 0]), "a ping");
        let mut unauthenticated = call(4, "mac", 501);
        unauthenticated[24..28].copy_from_slice(&0u32.to_be_bytes());
        assert!(!call_allowed(&unauthenticated, &[501, 0]), "AUTH_NULL");
        let mut reply = call(4, "mac", 501);
        reply[4..8].copy_from_slice(&1u32.to_be_bytes());
        assert!(!call_allowed(&reply, &[501, 0]), "not a call");
        assert!(call_allowed(&mount_call(100005, 1, "mac", 501), &[501, 0]), "MNT");
        assert!(!call_allowed(&mount_call(100005, 5, "mac", 501), &[501, 0]), "EXPORT names the secret");
        assert!(!call_allowed(&mount_call(100005, 2, "mac", 0), &[501, 0]), "DUMP");

        // Mount requests are answered by the relay: the secret path gets the root's
        // handle, once, and anything else is refused.
        let mnt = |path: &str| {
            let mut name = path.as_bytes().to_vec();
            name.resize(path.len().next_multiple_of(4), 0);
            [mount_call(100005, 1, "mac", 501), (path.len() as u32).to_be_bytes().to_vec(), name].concat()
        };
        assert!(is_mount_call(&mnt("/x")) && !is_mount_call(&call(1, "mac", 501)));
        let (root, mounted) = ([7u8; 24], AtomicBool::new(false));
        let refused = mount_reply(&mnt("/sandfs/ws/other"), "/sandfs/ws/0f3a", &root, &mounted);
        assert_eq!(refused[..8], [(0x8000_0000u32 | 28).to_be_bytes(), 42u32.to_be_bytes()].concat()[..]);
        assert_eq!(refused[8..], [1u32, 0, 0, 0, 0, 13].map(u32::to_be_bytes).concat()[..]);
        let granted = mount_reply(&mnt("/sandfs/ws/0f3a/"), "/sandfs/ws/0f3a", &root, &mounted);
        assert_eq!(granted[..4], (0x8000_0000u32 | 64).to_be_bytes());
        assert_eq!(granted[28..36], [0u32, 24].map(u32::to_be_bytes).concat()[..], "MNT3_OK and the handle's length");
        assert_eq!(granted[36..60], root);
        assert_eq!(granted[60..], [1u32, 1].map(u32::to_be_bytes).concat()[..], "AUTH_UNIX only");
        let again = mount_reply(&mnt("/sandfs/ws/0f3a"), "/sandfs/ws/0f3a", &root, &mounted);
        assert_eq!(again[28..], 13u32.to_be_bytes(), "only once");
        assert_eq!(
            mount_reply(&mount_call(100005, 1, "mac", 501), "/x", &root, &AtomicBool::new(false))[28..],
            13u32.to_be_bytes(),
            "no path"
        );

        // Unmounts are answered by the relay, from anyone, and never reach the server.
        assert!(is_unmount_call(&mount_call(100005, 3, "mac", 0)) && is_unmount_call(&mount_call(100005, 4, "x", 7)));
        assert!(!is_unmount_call(&mount_call(100005, 1, "mac", 501)) && !is_unmount_call(&call(3, "mac", 501)));
        let done = unmounted(&mount_call(100005, 3, "mac", 0));
        assert_eq!(done[..8], [(0x8000_0000u32 | 24).to_be_bytes(), 42u32.to_be_bytes()].concat()[..]);
        assert_eq!(done[8..], [1u32, 0, 0, 0, 0].map(u32::to_be_bytes).concat()[..]);

        let refused = refusal(&call(4, "mac", 502));
        assert_eq!(refused.len(), 24);
        assert_eq!(refused[..4], (0x8000_0000u32 | 20).to_be_bytes(), "one fragment of 20 bytes");
        assert_eq!(refused[4..8], 42u32.to_be_bytes(), "the call's xid");
        assert_eq!(refused[8..], [1u32, 1, 1, 5].map(u32::to_be_bytes).concat()[..]);
    }

    /// A successful ACCESS reply for a file of `kind` and `mode`, granting `access`.
    fn access_reply(kind: u32, mode: u32, access: u32) -> Vec<u8> {
        let head = [42u32, 1, 0, 0, 0, 0, 0, 1, kind, mode].map(u32::to_be_bytes).concat();
        [head, vec![0; 76], access.to_be_bytes().to_vec()].concat()
    }

    #[test]
    fn only_executable_files_get_execute_access() {
        assert!(is_access_call(&call(4, "mac", 501)));
        assert!(!is_access_call(&call(1, "mac", 501)), "GETATTR");

        let granted = |kind, mode| {
            let mut reply = access_reply(kind, mode, 0x3f);
            limit_access(&mut reply);
            u32::from_be_bytes(reply[reply.len() - 4..].try_into().unwrap())
        };
        assert_eq!(granted(1, 0o644), 0x1f, "a plain file");
        assert_eq!(granted(1, 0o755), 0x3f, "an executable");
        assert_eq!(granted(1, 0o610), 0x3f, "executable by its group");
        assert_eq!(granted(2, 0o644), 0x3f, "a directory");
        let mut failed = access_reply(1, 0o644, 0x3f);
        failed[28..32].copy_from_slice(&2u32.to_be_bytes());
        let before = failed.clone();
        limit_access(&mut failed);
        assert_eq!(failed, before, "an error reply is left alone");
        // A reply cut short is left alone rather than read past its end.
        let mut short = access_reply(1, 0o644, 0x3f)[..60].to_vec();
        limit_access(&mut short);
        assert_eq!(short, access_reply(1, 0o644, 0x3f)[..60]);
    }

    #[test]
    fn mount_options_name_the_server_port() {
        let options = mount_options(4049);
        assert!(options.contains("port=4049,mountport=4049") && options.contains("vers=3"), "{options}");
        assert!(options.split(',').any(|option| option == "sync"), "{options}");
    }
}
