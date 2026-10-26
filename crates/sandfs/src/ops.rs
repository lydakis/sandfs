//! What each operation of the public API does, over a [`store::Home`]. The `sandfs`
//! command is a thin layer over these; see [`crate::Home`].

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
#[cfg(mount)]
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use sandfs_core::VPath;

#[cfg(fuse)]
use crate::fs::SandFs;
#[cfg(mount)]
use crate::fs::{Inner, Shared};
#[cfg(nfs)]
use crate::nfs;
use crate::persist::{Saved, is_object_gone};
#[cfg(mount)]
use crate::server;
#[cfg(mount)]
use crate::store::Bases;
use crate::store::{self, Base, Home, MountInfo, ROOT, check_name, is_mounted};
use crate::{
    Change, Changes, ChangesOptions, CleanOutcome, Cleaned, ForkOptions, Forked, MountOptions, Mounted, ServeOptions,
    WorkspaceInfo, copy, folder, git, host, mcp, shell,
};

/// What `fork` copies.
enum Origin {
    /// A workspace, by name.
    Workspace(String),
    /// The working folder of a git repository sandfs doesn't serve.
    Checkout(PathBuf),
    /// A folder in neither.
    Folder(PathBuf),
}

/// What `fork FROM` copies: the workspace FROM names, or is mounted on or kept in, or
/// that holds the folder FROM; else the git repository whose working folder holds FROM; else the
/// folder itself.
fn fork_origin(home: &Home, from: &str) -> Result<Origin> {
    let not_found = match home.find(from) {
        Ok(name) => return Ok(Origin::Workspace(name)),
        Err(err) => err,
    };
    let Ok(path) = Path::new(from).canonicalize() else { return Err(not_found) };
    if !path.is_dir() {
        return Err(not_found);
    }
    let table = host::mount_table();
    if let Some(name) =
        path.ancestors().find_map(|dir| home.mounted_at(dir, &table).or_else(|| folder::find(home, dir)))
    {
        return Ok(Origin::Workspace(name));
    }
    Ok(git::working_folder(&path).map_or(Origin::Folder(path), Origin::Checkout))
}

pub(crate) fn mount_source(home: &Home, source: &str, dir: &Path, options: &MountOptions) -> Result<Mounted> {
    let MountOptions { name, read_only, foreground } = options;
    // A bare word naming a workspace remounts it; anything else is a source.
    if home.exists(source) && name.is_none() {
        only_when_created(read_only, source)?;
        let dir = mount(home, source, dir, *foreground)?;
        return Ok(Mounted { name: source.to_owned(), dir });
    }
    let name = default_name(name.clone(), dir)?;
    let repos = home.repos_lock(false)?;
    let mut record = home.resolve(source)?;
    record.read_only = relative_paths(read_only)?;
    home.create(&name, &record)?;
    drop(repos);
    let dir = mount_new(home, &name, dir, *foreground)?;
    Ok(Mounted { name, dir })
}

pub(crate) fn fork(home: &Home, from: &str, dir: Option<&Path>, options: &ForkOptions) -> Result<Forked> {
    let ForkOptions { name, read_only, branch, foreground, real_folder } = options;
    let origin = fork_origin(home, from)?;
    let read_only = relative_paths(read_only)?;
    // Where the files of a fork kept as a folder are cloned from, and the git index
    // that knows them: a clone, or another such fork.
    let clone_of = match &origin {
        Origin::Checkout(top) => Some((top.clone(), git::index_of(top))),
        Origin::Workspace(from) => home.record(from)?.folder.map(|top| (top, Some(folder::private_index(home, from)))),
        Origin::Folder(_) => None,
    };
    // By default only on macOS, where the mount is slowest, and only where nothing
    // needs a mount.
    let real = match real_folder {
        Some(true) if dir.is_none() => bail!("a fork kept as a folder needs the folder: give DIR"),
        Some(true) if clone_of.is_none() => {
            bail!("only a fork of a git repository's folder, or of such a fork, can be kept as a folder")
        }
        Some(true) if !read_only.is_empty() => {
            bail!("--read-only needs a mount, and a fork kept as a folder isn't one")
        }
        Some(real) => *real,
        None => cfg!(target_os = "macos") && dir.is_some() && clone_of.is_some() && read_only.is_empty(),
    };
    let name = match (dir, name.clone()) {
        (Some(dir), name) => default_name(name, dir)?,
        (None, Some(name)) => default_name(Some(name), Path::new(""))?,
        (None, None) => bail!("name the snapshot with --name, or give a directory to mount the fork on"),
    };
    let from = match origin {
        Origin::Workspace(from) => {
            sync(home, &from)?;
            let repos = home.repos_lock(false)?;
            let with_read_only = |mut record: store::Record| {
                // Its own folder, if any, is made below.
                record.folder = None;
                for path in &read_only {
                    if !record.read_only.contains(path) {
                        record.read_only.push(path.clone());
                    }
                }
                record
            };
            let target = home.create(&name, &with_read_only(home.record(&from)?))?;
            // `update` writes a new record before it drops the saved changes its
            // newer source has, so the record is read again after the changes
            // are copied: older changes over the newer record still read the
            // same, but newer changes over the older record would not.
            let made = Saved::new(home.workspace_dir(&from))
                .copy_to(&target)
                .and_then(|()| home.set_record(&name, &with_read_only(home.record(&from)?)));
            let made = made.and_then(|()| match branch {
                Some(branch) => git::switch_new(home, &name, branch),
                None => Ok(()),
            });
            let made = made.and_then(|()| home.copy_start(&from, &name));
            if let Err(err) = made {
                remove_unused(home, &name);
                return Err(err);
            }
            drop(repos);
            from
        }
        Origin::Checkout(top) => {
            let repos = home.repos_lock(false)?;
            let mut record = home.resolve_checkout(&top)?;
            record.read_only = read_only;
            home.create(&name, &record)?;
            if let Err(err) = git::start(home, &name, branch.as_deref()) {
                remove_unused(home, &name);
                return Err(err);
            }
            drop(repos);
            top.display().to_string()
        }
        Origin::Folder(folder) => {
            if branch.is_some() {
                bail!("--branch needs a git repository, and {} is a plain folder", folder.display());
            }
            let repos = home.repos_lock(false)?;
            let mut record = home.freeze(&folder)?;
            record.read_only = read_only;
            if let Err(err) = home.create(&name, &record) {
                if let Base::Dir { path } = &record.base {
                    let _ = store::remove_copy(path);
                }
                return Err(err);
            }
            drop(repos);
            record.source
        }
    };
    let real_folder = real && dir.is_some();
    let dir = match (dir, clone_of) {
        (Some(dir), Some((top, index))) if real => {
            Some(folder::make(home, &name, &top, dir, index.as_deref()).inspect_err(|_| remove_unused(home, &name))?)
        }
        (Some(dir), _) => Some(mount_new(home, &name, dir, *foreground).inspect_err(|_| {
            // A failed mount takes the workspace with it, and so its copy of a folder.
            if let Ok(_creating) = home.repos_lock(true) {
                let _ = home.remove_unused_copies();
            }
        })?),
        (None, _) => None,
    };
    Ok(Forked { name, from, dir, real_folder })
}

pub(crate) fn changes(home: &Home, workspace: &str, options: &ChangesOptions) -> Result<Changes> {
    let ChangesOptions { paths, since, copy_to, force } = options;
    let name = home.find(workspace)?;
    sync(home, &name)?;
    let since = since.as_deref().map(|since| home.find(since)).transpose()?;
    if let Some(since) = &since {
        sync(home, since)?;
    }
    // Comparing reads the source, so `sandfs clean` waits until this is done.
    let _reading = home.repos_lock(false)?;
    let root = VPath::parse(ROOT)?;
    let wanted = paths
        .iter()
        .map(|path| {
            let full = VPath::parse(&format!("{ROOT}/{}", path.trim_start_matches("./")))?;
            if !full.starts_with(&root) {
                bail!("{path} is outside the workspace");
            }
            Ok(full)
        })
        .collect::<Result<Vec<_>>>()?;
    // Saved files are read as needed, and a mount saving meanwhile may replace
    // one; reading again from the newer save works. A copy interrupted that way
    // starts over, writing what it already wrote again.
    let mut attempts = 0;
    loop {
        let copy = copy_to.as_deref().map(|dir| (dir, *force));
        match changes_and_copy(home, &name, since.as_deref(), &wanted, copy) {
            Ok(found) => return Ok(found),
            Err(err) if is_object_gone(&err) && attempts < 10 => attempts += 1,
            Err(err) => return Err(err),
        }
    }
}

pub(crate) fn workspaces(home: &Home) -> Result<Vec<WorkspaceInfo>> {
    let table = host::mount_table();
    let mut found = Vec::new();
    for name in home.names()? {
        let record = home.record(&name)?;
        let mounted = home.mount_info_in(&name, &table).map(|info| info.mountpoint).or(record.folder);
        found.push(WorkspaceInfo { name, source: record.source, mounted });
    }
    Ok(found)
}

pub(crate) fn unmount_workspace(home: &Home, workspace: &str) -> Result<()> {
    let name = home.find(workspace)?;
    if let Some(dir) = home.record(&name)?.folder {
        bail!("{name} is kept as the folder {}, not mounted; `sandfs rm --unmount {name}` deletes it", dir.display());
    }
    let info = home.attached_mount(&name).with_context(|| format!("{name} is not mounted"))?;
    unmount(home, &name, &info)
}

pub(crate) fn remove(home: &Home, name: &str, first: bool) -> Result<()> {
    let name = if first { home.find(name)? } else { name.to_owned() };
    if let Some(info) = home.attached_mount(&name) {
        if !first {
            bail!("{name} is mounted; unmount it first, or pass --unmount");
        }
        unmount(home, &name, &info)?;
    }
    let record = home.record(&name)?;
    if let Some(dir) = &record.folder
        && !first
    {
        bail!("{name} is kept as the folder {}; pass --unmount to delete the folder too", dir.display());
    }
    let writer = home.lock(&name, "sandfs rm")?;
    if let Some(dir) = &record.folder {
        folder::remove(home, &name, dir)?;
    }
    home.remove(&name)?;
    drop(writer);
    // A copy of a folder goes with the last workspace over it.
    let _creating = home.repos_lock(true)?;
    home.remove_unused_copies()
}

pub(crate) fn serve_mcp(home: &Home, workspace: &str, options: &ServeOptions) -> Result<()> {
    let ServeOptions { name, read_only } = options;
    // As with `mount`, a bare word naming a workspace serves it; anything else
    // is a source. This is how workspaces are made where nothing mounts.
    if home.exists(workspace) && name.is_none() {
        only_when_created(read_only, workspace)?;
        not_a_folder(home, workspace)?;
        return mcp::serve(home, workspace);
    }
    // Named after the resolved folder or repository, so `.` and `repo@v1` work.
    let repos = home.repos_lock(false)?;
    let mut record = home.resolve(workspace)?;
    record.read_only = relative_paths(read_only)?;
    let name = default_name(name.clone(), &source_dir(&record.base))?;
    home.create(&name, &record)?;
    drop(repos);
    mcp::serve(home, &name)
}

pub(crate) fn shell(home: &Home, workspace: &str, network: bool, command: &[OsString]) -> Result<i32> {
    let name = home.find(workspace)?;
    if home.record(&name)?.folder.is_some() {
        bail!("{name} is kept as a folder, which can't be confined; a fork made with --mount can");
    }
    let info = home
        .attached_mount(&name)
        .with_context(|| format!("{name} is not mounted; mount it first with sandfs mount {name} DIR"))?;
    // A git workspace's .git reads its objects from the local copy, by absolute path.
    let (source, readable) = match home.record(&name)?.base {
        Base::Dir { path } => (path, Vec::new()),
        Base::Git { repo, .. } => {
            let objects = git::object_folders(&repo)?;
            (repo, objects)
        }
    };
    let command = command.to_vec();
    let confined = shell::Confined { dir: info.mountpoint, source: Some(source), readable, network, command };
    shell::run(&confined)
}

/// `sandfs git-fsmonitor`: tell git which files of the mounted workspace it runs in
/// have changed.
pub(crate) fn git_fsmonitor(home: &Home, version: &str) -> Result<()> {
    // Git runs its file monitor at the root of the checkout: the mount.
    let root = std::env::current_dir()?;
    // A git worktree added from a mount shares the mount's config, this monitor
    // included, but is a plain folder: there any file may have changed, and git looks.
    let Some(name) = root.to_str().and_then(|root| home.mounted(root)) else { return git::fsmonitor_all(version) };
    sync(home, &name)?;
    git::fsmonitor(&Saved::new(home.workspace_dir(&name)).paths()?, version)
}

/// The folder a source's name comes from: `data` for `/srv/data`, `/srv/data/.git` or a
/// cached `github.com/team/data.git`.
fn source_dir(base: &Base) -> PathBuf {
    match base {
        Base::Dir { path } => path.clone(),
        Base::Git { repo, .. } if repo.file_name().is_some_and(|name| name == ".git") => {
            repo.parent().map_or_else(|| repo.clone(), Path::to_path_buf)
        }
        Base::Git { repo, .. } if repo.extension().is_some_and(|ext| ext == "git") => repo.with_extension(""),
        Base::Git { repo, .. } => repo.clone(),
    }
}

fn default_name(name: Option<String>, dir: &Path) -> Result<String> {
    let name = match name {
        Some(name) => name,
        None => dir.file_name().and_then(|n| n.to_str()).context("give the workspace a --name")?.to_owned(),
    };
    check_name(&name)?;
    Ok(name)
}

/// Ask a mounted workspace to save everything written so far, or bring one kept as a
/// folder up to date with it. `fsync` on a FUSE
/// mount's root directory makes the serving process save before it returns; an NFS
/// mount's process saves when asked on its save port, since `fsync` never reaches it.
fn sync(home: &Home, name: &str) -> Result<()> {
    if let Some(dir) = home.record(name)?.folder {
        return folder::sync(home, name, &dir);
    }
    let Some(info) = home.mount_info(name) else { return Ok(()) };
    // A dead server's pid may belong to another process by now. Only the process holding
    // the workspace serves it; without one, the last save is all there is to read.
    #[cfg(mount)]
    if !home.locked_by(name, info.pid) {
        return Ok(());
    }
    let saved = match (info.save_port, info.save_secret) {
        #[cfg(nfs)]
        (Some(port), Some(secret)) => nfs::request_save(port, &secret),
        _ => std::fs::File::open(&info.mountpoint).and_then(|dir| dir.sync_all()).map_err(anyhow::Error::from),
    };
    saved.with_context(|| format!("saving {name} before reading it"))
}

/// The changes of workspace `name` (against `since`, or its source) under any of
/// `wanted`, as their kinds and paths relative to the workspace, after writing them
/// into the folder of `copy_to` if given (forcing over files changed there with its flag).
fn changes_and_copy(
    home: &Home,
    name: &str,
    since: Option<&str>,
    wanted: &[VPath],
    copy_to: Option<(&Path, bool)>,
) -> Result<Changes> {
    // What the changes are made to, which a file where they are copied may differ from
    // only where it was changed there since.
    let (opened, start, changes) = match since {
        Some(since) => {
            let (opened, base) = home.open_pair(name, since)?;
            let changes = opened.ws.changes_since(&base.ws)?;
            (opened, base.ws, changes)
        }
        None => {
            let (opened, start) = home.open_with_start(name)?;
            let changes = opened.ws.changes()?;
            (opened, start, changes)
        }
    };
    let root = VPath::parse(ROOT)?;
    // What git keeps in a git mount's `.git` would drown out the files themselves.
    let git_dir = matches!(opened.record.base, Base::Git { .. }).then(|| VPath::parse(&format!("{ROOT}/.git")));
    let git_dir = git_dir.transpose()?;
    // But git runs the programs `.git/config` and `.git/hooks` name, so a change
    // there is shown, apart from the files, for whoever reviews the workspace.
    let settings = match &opened.record.folder {
        Some(dir) => folder::git_settings(home, name, dir),
        None => changes
            .iter()
            .filter(|change| {
                let rest = git_dir.as_ref().and_then(|dir| change.path.strip_prefix(dir));
                rest.is_some_and(|rest| matches!(rest.first(), Some(&"config" | &"hooks")))
            })
            .map(|change| change.path.strip_prefix(&root).unwrap_or_default().join("/"))
            .collect(),
    };
    let listed: Vec<_> = changes
        .iter()
        .filter(|change| !git_dir.as_ref().is_some_and(|dir| change.path.starts_with(dir)))
        .filter(|change| wanted.is_empty() || wanted.iter().any(|path| change.path.starts_with(path)))
        .map(|change| (change, change.path.strip_prefix(&root).unwrap_or_default().join("/")))
        .collect();
    let mut held_back = 0;
    if let Some((dir, force)) = copy_to {
        let items: Vec<_> =
            listed.iter().map(|(change, rel)| copy::Item { path: &change.path, rel, kind: change.kind }).collect();
        let start = (!force).then_some(&start);
        held_back = copy::copy_to(&opened.ws, start, &items, dir)?;
    }
    let changes = listed.into_iter().map(|(change, path)| Change { kind: change.kind, path }).collect();
    Ok(Changes { changes, held_back, git_settings: settings })
}

/// `--read-only` paths as a workspace records them: relative to the source, checked.
fn relative_paths(paths: &[String]) -> Result<Vec<String>> {
    paths
        .iter()
        .map(|path| {
            let rel = path.trim_start_matches("./").trim_matches('/');
            let full = VPath::parse(&format!("{ROOT}/{rel}")).with_context(|| format!("--read-only {path}"))?;
            match full.strip_prefix(&VPath::parse(ROOT)?) {
                Some(parts) => Ok(parts.join("/")),
                None => bail!("--read-only {path} is outside the source"),
            }
        })
        .collect()
}

/// Read-only paths are part of a workspace from its start, so its changes never
/// include any there.
fn only_when_created(read_only: &[String], name: &str) -> Result<()> {
    if !read_only.is_empty() {
        bail!("--read-only applies when a workspace is created, and {name} already exists");
    }
    Ok(())
}

/// Refuse a workspace kept as a folder (see `folder`), whose files are only there.
pub(crate) fn not_a_folder(home: &Home, name: &str) -> Result<()> {
    if let Some(dir) = home.record(name)?.folder {
        bail!("{name} is kept as the folder {}; work there, or fork it with --mount for a mount", dir.display());
    }
    Ok(())
}

/// Mount a workspace that was just created, removing it again if the mount fails so
/// the same command can simply be retried.
fn mount_new(home: &Home, name: &str, dir: &Path, foreground: bool) -> Result<PathBuf> {
    // A fork starts with a copy of its parent's changes.json. Every mount saves when it
    // ends, so an index that is still the initial one means the mount never ran.
    let index = home.workspace_dir(name).join("changes.json");
    let initial = std::fs::read(&index).ok();
    let result = mount(home, name, dir, foreground);
    if result.is_err() && std::fs::read(&index).ok() == initial {
        remove_unused(home, name);
    }
    result
}

/// Remove a workspace that was just created, unless another process has started
/// serving it meanwhile.
fn remove_unused(home: &Home, name: &str) {
    if let Ok(_writer) = home.lock(name, "sandfs, removing a workspace whose mount failed") {
        let _ = home.remove(name);
    }
}

/// Mount `name` at `dir`: serve it here with `foreground`, otherwise start a
/// background process and wait until the mount is up.
#[cfg(mount)]
fn mount(home: &Home, name: &str, dir: &Path, foreground: bool) -> Result<PathBuf> {
    not_a_folder(home, name)?;
    if let Some(info) = home.attached_mount(name) {
        let at = info.mountpoint.display();
        if host::alive(info.pid) {
            bail!("{name} is already mounted at {at}");
        }
        // Mounting again would forget where the old mount is, and nothing could detach it.
        bail!("{name} is still attached at {at}, whose process has exited; `sandfs unmount {name}` detaches it");
    }
    // The source is read through its host path, so a mount on top of it or inside it
    // would read itself (and create the mount point inside a read-only source).
    if let Base::Dir { path } = home.record(name)?.base {
        // Opening the source canonicalizes its path again, so compare with that.
        let path = path.canonicalize().unwrap_or(path);
        let target = resolve_new(dir)?;
        if target.starts_with(&path) || path.starts_with(&target) {
            bail!("{} overlaps the source folder {}; mount it somewhere else", target.display(), path.display());
        }
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let dir = dir.canonicalize()?;
    if is_mounted(&dir) {
        bail!("something is already mounted at {}", dir.display());
    }
    if foreground {
        serve(home, name, &dir, &Arc::default(), || {})?;
        return Ok(dir);
    }
    server::mount(home, name, &dir)?;
    Ok(dir)
}

/// `sandfs clean`: shrink each local copy of a remote repository to what its
/// workspaces need, and delete those none uses.
pub(crate) fn clean(home: &Home) -> Result<Vec<Cleaned>> {
    let _creating = home.repos_lock(true)?;
    home.remove_unused_copies()?;
    let repos = home.root().join("repos");
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    // The workspaces over each local copy, and the commits they are of.
    let mut users: BTreeMap<PathBuf, Vec<(String, String)>> = BTreeMap::new();
    for name in home.names()? {
        if let Base::Git { repo, commit, .. } = home.record(&name)?.base {
            users.entry(canonical(&repo)).or_default().push((name, commit));
        }
    }
    let mut cleaned = Vec::new();
    for copy in git::local_copies(&repos)? {
        let repository = copy.strip_prefix(&repos).unwrap_or(&copy).display().to_string();
        let before = disk_usage(&copy);
        let Some(users) = users.get(&canonical(&copy)) else {
            std::fs::remove_dir_all(&copy).with_context(|| format!("removing {}", copy.display()))?;
            cleaned.push(Cleaned { repository, outcome: CleanOutcome::Deleted { freed: before } });
            continue;
        };
        // Held until the copy is shrunk, so no workspace over it mounts in the meantime.
        let mut locks = Vec::new();
        for (name, _) in users {
            match home.lock(name, "sandfs clean") {
                Ok(lock) => locks.push(lock),
                Err(_) => break,
            }
        }
        if locks.len() < users.len() {
            let busy = users[locks.len()].0.clone();
            cleaned.push(Cleaned { repository, outcome: CleanOutcome::Kept { busy } });
            continue;
        }
        let keep = users.iter().map(|(_, commit)| commit.clone()).collect();
        git::drop_contents(&copy, &keep).with_context(|| format!("cleaning {repository}"))?;
        let freed = before.saturating_sub(disk_usage(&copy));
        cleaned.push(Cleaned { repository, outcome: CleanOutcome::Shrunk { freed } });
    }
    Ok(cleaned)
}

/// The bytes the files under `path` take up.
fn disk_usage(path: &Path) -> u64 {
    let mut total = 0;
    let mut folders = vec![path.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            match entry.metadata() {
                Ok(meta) if meta.is_dir() => folders.push(entry.path()),
                Ok(meta) => total += meta.len(),
                Err(_) => {}
            }
        }
    }
    total
}

/// Serve every mount asked of this home's background process (see `server`).
#[cfg(mount)]
pub(crate) fn serve_mounts(home: Home, serve: bool) -> Result<()> {
    if serve { server::run(home) } else { server::detach() }
}

/// The canonical path `dir` will have once created: its nearest existing ancestor,
/// resolved, plus the components that do not exist yet.
pub(crate) fn resolve_new(dir: &Path) -> Result<PathBuf> {
    let absolute = std::path::absolute(dir)?;
    let mut rest = Vec::new();
    let mut existing = absolute.as_path();
    while !existing.exists() {
        rest.push(existing.file_name().context("no existing parent directory")?);
        existing = existing.parent().context("no existing parent directory")?;
    }
    let mut resolved = existing.canonicalize()?;
    resolved.extend(rest.into_iter().rev());
    Ok(resolved)
}

/// Serve `name` at `dir` until it is unmounted, then save its changes. `ready` is called
/// once the mount is up, and never if mounting fails. Workspaces over a commit share it
/// through `bases` with the others this process serves.
#[cfg(mount)]
pub(crate) fn serve(home: &Home, name: &str, dir: &Path, bases: &Arc<Bases>, ready: impl FnOnce()) -> Result<()> {
    let pid = std::process::id();
    let _writer = home.lock(name, &format!("the mount at {} (process {pid})", dir.display()))?;
    // Two workspaces mounted on one directory at once would stack. Locking the directory
    // itself (the one underneath, which a mount covers) lets only one mount proceed;
    // whoever comes after it sees the mount.
    let point = std::fs::File::open(dir).with_context(|| format!("opening {}", dir.display()))?;
    if point.try_lock().is_err() || is_mounted(dir) {
        bail!("something is already mounted at {}", dir.display());
    }
    // A failed final save is reported by the `unmount` that asked for it, not a later one.
    let save_error = home.workspace_dir(name).join(SAVE_ERROR);
    let _ = std::fs::remove_file(&save_error);
    let (mut opened, base) = home.open_shared(name, bases)?;
    opened.saved.collect_garbage();
    let owner = std::os::unix::fs::MetadataExt::uid(&std::fs::metadata(dir)?);
    let group = std::os::unix::fs::MetadataExt::gid(&std::fs::metadata(dir)?);
    let base_time = UNIX_EPOCH + Duration::from_secs(opened.record.created);
    let root = VPath::parse(ROOT)?;
    let times = served_times(&opened);
    let mut inner = Inner::new(opened.ws, &root, times, opened.restored.dirs, base_time, (owner, group));
    inner.ranged_reads = matches!(opened.record.base, Base::Dir { .. });
    inner.drafts = Some(opened.saved.drafts());
    // NFS has no close to flush a file's writes, and macOS's NFS client adds files of
    // its own next to the ones people write.
    inner.keep_removed = !cfg!(nfs);
    inner.renumber_changed = cfg!(nfs);
    inner.skip_bookkeeping = cfg!(nfs);
    let shared = Shared::new(inner, opened.saved, root);
    // Listed for `update`, which moves the workspace while it stays mounted.
    let serving = server::serving(name, shared.clone(), base);

    // A mount that never came up has nothing to save, and saving would make a new
    // workspace look used to the `mount_new` that removes it again.
    let was_mounted = std::cell::Cell::new(false);
    let mut ready = Some(ready);
    let mut mounted = |save: Option<(u16, String)>| -> Result<()> {
        let (save_port, save_secret) = save.unzip();
        let info = MountInfo { pid, mountpoint: dir.to_path_buf(), save_port, save_secret, id: mount_id() };
        home.set_mount_info(name, Some(&info))?;
        was_mounted.set(true);
        eprintln!("sandfs: serving {name} at {} since {:?}", dir.display(), SystemTime::now());
        if let Some(ready) = ready.take() {
            ready();
        }
        Ok(())
    };
    #[cfg(fuse)]
    let result = {
        let mut config = fuser::Config::default();
        config.mount_options = host::mount_options(name);
        config.acl = host::mount_acl();
        let session = match fuser::Session::new(SandFs(shared.clone()), dir, &config) {
            Ok(session) => session,
            // A fusermount that reads its settings differently refuses root's access;
            // the mount is still worth having for its owner.
            Err(err) if config.acl != fuser::SessionACL::Owner => {
                eprintln!("sandfs: mounting {name} for root too failed ({err}); mounting it for its owner only");
                config.acl = fuser::SessionACL::Owner;
                fuser::Session::new(SandFs(shared.clone()), dir, &config).context("mounting with FUSE")?
            }
            Err(err) => return Err(err).context("mounting with FUSE"),
        };
        shared.notify_stale_listings(session.notifier());
        mounted(None)?;
        session.run().map_err(anyhow::Error::from)
    };
    #[cfg(nfs)]
    let result = nfs::run(name, dir, &shared, |port, secret| mounted(Some((port, secret))));

    shared.stop();
    if !was_mounted.get() {
        return result.context("mounting");
    }
    let saved = shared.save(true);
    match &saved {
        Ok(()) => home.set_mount_info(name, None)?,
        Err(err) => {
            // `unmount` is waiting for mount.json to go. A full disk may refuse a new
            // file but not a rename, so the failure is signalled by moving mount.json
            // to save-error, and the reason is added if it can be.
            let _ = std::fs::rename(home.workspace_dir(name).join("mount.json"), &save_error);
            let _ = std::fs::write(&save_error, format!("{err:#}"));
        }
    }
    serving.end(home);
    result.context("serving the mount")?;
    saved.context("saving changes at unmount")
}

/// The modification times a mount of `opened` reports for the paths that don't keep
/// their source's: the saved ones, and base files a newer source changed, which
/// report when the workspace moved onto it.
#[cfg(mount)]
pub(crate) fn served_times(opened: &store::Opened) -> BTreeMap<String, SystemTime> {
    let mut times = opened.restored.times.clone();
    for (rel, (secs, nanos)) in &opened.record.updated {
        times.entry(format!("{ROOT}/{rel}")).or_insert(UNIX_EPOCH + Duration::new(*secs, *nanos));
    }
    times
}

/// A number for a new mount that differs from the ones before it.
#[cfg(mount)]
fn mount_id() -> u64 {
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos() as u64;
    now ^ COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Where a mount's process leaves the error of a final save that failed.
#[cfg(mount)]
const SAVE_ERROR: &str = "save-error";

/// Unmount `name` and wait for the serving process to save, which it signals by
/// removing mount.json, and to let go of the workspace.
#[cfg(mount)]
fn unmount(home: &Home, name: &str, info: &MountInfo) -> Result<()> {
    let state = home.workspace_dir(name);
    // One process serves every mount, so the workspace may be mounted again by it while
    // this waits; mount.json's id says whether the mount recorded is still this one.
    let recorded = || home.recorded_mount(name).map(|current| current.id);
    let replaced = || recorded().is_some_and(|id| id != info.id);
    // The serving process holds the workspace; checking that too means a process that
    // has since taken over a dead server's pid isn't waited for.
    let serving = || host::alive(info.pid) && home.locked_by(name, info.pid) && !replaced();
    host::unmount(&info.mountpoint, !serving())?;
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut wait = Duration::from_millis(1);
    while recorded() == Some(info.id) && serving() {
        if Instant::now() >= deadline {
            bail!("unmounted, but process {} is still saving the workspace's changes", info.pid);
        }
        std::thread::sleep(wait);
        wait = (wait * 2).min(Duration::from_millis(50));
    }
    if recorded() == Some(info.id) {
        // The serving process died without cleaning up; its last save is what remains.
        eprintln!("sandfs: the mount's process had exited; changes since its last save are lost");
        let _ = std::fs::remove_file(state.join("mount.json"));
    }
    // The process lets go of the workspace just after removing mount.json. Wait for
    // that too, so a command right after this one finds the workspace free.
    while serving() {
        if Instant::now() >= deadline {
            bail!("unmounted, but process {} has not let go of {name}", info.pid);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if let Ok(error) = std::fs::read_to_string(state.join(SAVE_ERROR)) {
        let _ = std::fs::remove_file(state.join(SAVE_ERROR));
        // Still the moved mount.json (or empty) when the reason could not be written.
        let reason = if error.trim().is_empty() || error.starts_with('{') {
            format!("see {}", server::log_path(home).display())
        } else {
            error
        };
        bail!("unmounted, but saving the last changes failed, so changes since the last save are lost: {reason}");
    }
    Ok(())
}

// Without a mount (Windows, or a build without the `mount` feature) everything works
// but mounting; `serve --mcp SOURCE` creates workspaces instead.
#[cfg(not(mount))]
const NO_MOUNT: &str =
    "this build of sandfs can't mount (that needs Linux or macOS); `sandfs serve --mcp SOURCE` works here";

#[cfg(not(mount))]
fn mount(_home: &Home, _name: &str, _dir: &Path, _foreground: bool) -> Result<PathBuf> {
    bail!(NO_MOUNT)
}

#[cfg(not(mount))]
pub(crate) fn serve_mounts(_home: Home, _serve: bool) -> Result<()> {
    bail!(NO_MOUNT)
}

#[cfg(not(mount))]
fn unmount(_home: &Home, _name: &str, _info: &MountInfo) -> Result<()> {
    bail!(NO_MOUNT)
}
