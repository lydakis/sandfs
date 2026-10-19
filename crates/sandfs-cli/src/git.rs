//! Git commits as sources: `git:<path or url>[@rev]`.
//!
//! Everything git-specific in the CLI lives here, so the rest of it only knows host
//! folders. Remote repositories become bare partial clones under `<home>/repos` that
//! hold only the commits that were mounted, without their history, so a mount costs
//! one request for the commit and its folder listings. File contents are fetched only
//! when a directory or file is first read.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use sandfs_core::Source;
use sandfs_git::GitSource;

use crate::store::expand;

/// Resolve `spec` (what follows `git:`) to a local repository and a commit id,
/// cloning or fetching remote repositories into `repos`.
pub fn resolve(repos: &Path, spec: &str) -> Result<(PathBuf, String)> {
    let (location, rev) = split_rev(spec);
    if location.is_empty() || rev.as_deref() == Some("") {
        bail!("incomplete source \"git:{spec}\"");
    }
    let rev = rev.as_deref().unwrap_or("HEAD");
    if rev.starts_with('-') {
        bail!("invalid revision {rev:?}");
    }
    if is_url(location) {
        return fetch_commit(repos, location, rev);
    }
    let repo = expand(location).canonicalize().with_context(|| format!("no repository at {location}"))?;
    Ok((repo.clone(), commit_of(&repo, rev)?))
}

fn commit_of(repo: &Path, rev: &str) -> Result<String> {
    git(repo, &["rev-parse", "--verify", "--end-of-options", &format!("{rev}^{{commit}}")])
        .with_context(|| format!("{rev:?} is not a commit in {}", repo.display()))
}

/// The source for `commit` of `repo`.
pub fn open(repo: &Path, commit: &str) -> Result<Arc<dyn Source>> {
    // Sizes let FUSE answer `stat` without reading whole files.
    Ok(Arc::new(GitSource::open(repo, commit)?.with_sizes()?))
}

/// Fetch `rev` of `url` into its local partial clone, creating the clone on first use,
/// and return the clone and the commit id.
///
/// Only the commit itself and its folder listings are fetched (depth 1, no file
/// contents), which takes about a second even for a repository with years of history.
/// A full commit id that was mounted before is not looked up again, so it mounts
/// offline; a branch or tag name is looked up on the server every time.
fn fetch_commit(repos: &Path, url: &str, rev: &str) -> Result<(PathBuf, String)> {
    let dest = repos.join(cache_path(url)?);
    let existed = dest.join("HEAD").exists();
    if !existed {
        eprintln!("sandfs: fetching {url} (file contents are fetched when first read)");
        create_clone(&dest, url)?;
    } else if let Some(commit) = pinned(&dest, rev) {
        // Nothing to fetch, but later file contents come from `origin`: switch it to a
        // different URL given now only if that URL answers, so working offline still works.
        if origin(&dest).as_deref() != Some(url) && git(&dest, &["ls-remote", url, "HEAD"]).is_ok() {
            git(&dest, &["remote", "set-url", "origin", url])?;
        }
        return Ok((dest, commit));
    }
    // A ref of this fetch's own, so concurrent mounts of one repository don't race on
    // FETCH_HEAD or on each other's results.
    static FETCHES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = FETCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let fetched = format!("refs/sandfs/fetch-{}-{n}", std::process::id());
    let refspec = format!("+{rev}:{fetched}");
    let args =
        ["fetch", "-q", "--depth=1", "--filter=blob:none", "--no-tags", "--recurse-submodules=no", url, &refspec];
    // A shallow fetch rewrites the local copy's list of shallow commits, and git refuses
    // to while another fetch is doing so ("shallow.lock: File exists", "shallow file has
    // changed since we read it"). Of two mounts fetching at once, one waits and retries.
    let mut waited = 0;
    let fetch = loop {
        match git(&dest, &args) {
            Err(err) if waited < 300 && is_shallow_race(&format!("{err:#}")) => {
                waited += 1;
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            result => break result,
        }
    };
    let commit = match fetch {
        Ok(_) => {
            // One local copy serves every URL that names the repository (https or ssh,
            // with or without a user), and file contents come from `origin`. Fetching from
            // `url` itself and only then making it `origin` replaces an earlier URL that
            // didn't work, but never one that did with one that doesn't.
            if origin(&dest).as_deref() != Some(url) {
                git(&dest, &["remote", "set-url", "origin", url])?;
            }
            let commit = commit_of(&dest, &fetched);
            let _ = git(&dest, &["update-ref", "-d", &fetched]);
            commit?
        }
        // Offline, or a revision the server can't name (`main~3`). A commit id mounted
        // before has already returned above, so only the server could answer this one.
        Err(err) => {
            let hint = if is_commit_id(rev) {
                ""
            } else {
                " (a branch or tag name is looked up on the server; offline, only a full commit id that was mounted before mounts)"
            };
            return Err(err).with_context(|| format!("could not fetch {rev:?} from {url}{hint}"));
        }
    };
    // Keep every mounted commit reachable, so git's maintenance never removes one a
    // workspace still reads from.
    git(&dest, &["update-ref", &format!("refs/sandfs/commits/{commit}"), &commit])?;
    Ok((dest, commit))
}

/// An empty bare repository whose `origin` is `url`, set up as a partial clone.
fn create_clone(dest: &Path, url: &str) -> Result<()> {
    let parent = dest.parent().context("bad cache path")?;
    std::fs::create_dir_all(parent)?;
    // Set up beside the destination and rename, so an interrupted setup leaves nothing behind.
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("repo"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    let tmp_str = tmp.to_str().context("non-UTF-8 home directory")?;
    let setup = git(Path::new("."), &["init", "-q", "--bare", tmp_str])
        .and_then(|_| git(&tmp, &["remote", "add", "origin", url]))
        .and_then(|_| git(&tmp, &["config", "remote.origin.promisor", "true"]))
        .and_then(|_| git(&tmp, &["config", "remote.origin.partialclonefilter", "blob:none"]));
    if let Err(err) = setup {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(err).context("setting up the local copy");
    }
    if let Err(err) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_dir_all(&tmp);
        // Another mount of the same URL may have finished first.
        if !dest.join("HEAD").exists() {
            return Err(err).context("moving the local copy into place");
        }
    }
    Ok(())
}

/// The commit `rev` names if it is a full commit id that was mounted before.
///
/// This checks the pin ref rather than the object: in a partial clone, asking git
/// about a commit it lacks makes it fetch that commit with all of its history.
fn pinned(repo: &Path, rev: &str) -> Option<String> {
    let id = rev.to_ascii_lowercase();
    let pin = format!("refs/sandfs/commits/{id}");
    (is_commit_id(&id) && git(repo, &["show-ref", "--verify", "--quiet", &pin]).is_ok()).then_some(id)
}

/// The URL the local copy fetches file contents from.
fn origin(repo: &Path) -> Option<String> {
    git(repo, &["config", "remote.origin.url"]).ok()
}

/// Whether git refused a fetch only because another fetch was rewriting the local copy's
/// list of shallow commits at the same time.
fn is_shallow_race(error: &str) -> bool {
    error.contains("shallow.lock") || error.contains("shallow file has changed since we read it")
}

/// A full commit id, which names the same commit forever.
fn is_commit_id(rev: &str) -> bool {
    matches!(rev.len(), 40 | 64) && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Split a trailing `@rev`, which may contain slashes (`@feature/x`) or a reflog
/// selector (`@HEAD@{1}`). The `@` of a URL's user (`git@host:repo`,
/// `https://me@host/repo`) is left alone.
fn split_rev(spec: &str) -> (&str, Option<String>) {
    let Some(at) = spec.match_indices('@').map(|(i, _)| i).rfind(|&i| !spec[i + 1..].starts_with('{')) else {
        return (spec, None);
    };
    let (location, rev) = (&spec[..at], &spec[at + 1..]);
    // Ref names cannot contain ':', which rules out the host of `user@host:repo`, and
    // an https user is followed by a host with no path before it (`file://C:\repo` on
    // Windows has one).
    let user_of_url = location.split_once("://").is_some_and(|(_, rest)| !rest.contains(['/', '\\']));
    if at == 0 || rev.contains(':') || user_of_url { (spec, None) } else { (location, Some(rev.to_owned())) }
}

/// A URL, or scp-style `[user@]host:path`, as git reads it: a ':' before any '/'.
fn is_url(location: &str) -> bool {
    location.contains("://") || location.find(':').is_some_and(|colon| !location[..colon].contains('/'))
}

/// `https://github.com/owner/repo.git` becomes `github.com/owner/repo.git`.
fn cache_path(url: &str) -> Result<PathBuf> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host).replacen(':', "/", 1);
    let rest = rest.trim_end_matches('/').trim_end_matches(".git");
    let mut path = PathBuf::new();
    for part in rest.split('/').filter(|part| !part.is_empty()) {
        let clean: String =
            part.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
        if clean.starts_with('.') {
            bail!("cannot cache {url:?}");
        }
        path.push(clean);
    }
    if path.as_os_str().is_empty() {
        bail!("cannot cache {url:?}");
    }
    path.set_extension("git");
    Ok(path)
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .context("running git")?;
    if !output.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revisions_split_off_paths_and_urls() {
        let split = |spec| {
            let (location, rev) = split_rev(spec);
            (location.to_owned(), rev)
        };
        assert_eq!(split("~/src/sandfs@v1.0"), ("~/src/sandfs".into(), Some("v1.0".into())));
        assert_eq!(split("git@github.com:o/r.git"), ("git@github.com:o/r.git".into(), None));
        assert_eq!(split("git@github.com:o/r.git@dev"), ("git@github.com:o/r.git".into(), Some("dev".into())));
        assert_eq!(split("https://u@host/r"), ("https://u@host/r".into(), None));
        assert_eq!(split("git@host:repo"), ("git@host:repo".into(), None));
        assert_eq!(split("/srv/repo@feature/foo"), ("/srv/repo".into(), Some("feature/foo".into())));
        assert_eq!(split("https://u@host/r@release/1.0"), ("https://u@host/r".into(), Some("release/1.0".into())));
        assert_eq!(split("/srv/repo@HEAD@{1}"), ("/srv/repo".into(), Some("HEAD@{1}".into())));
        assert_eq!(split(r"file://C:\src\repo@v1"), (r"file://C:\src\repo".into(), Some("v1".into())));
        assert_eq!(split("/srv/repo@main@{2.days.ago}"), ("/srv/repo".into(), Some("main@{2.days.ago}".into())));
        assert!(resolve(Path::new("/nonexistent"), "repo@").is_err());
    }

    /// Run git in `dir` for a test, with an identity for commits.
    fn run(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-C"])
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    #[test]
    fn remote_mounts_fetch_only_the_mounted_commit() {
        // The cache refuses URL parts that start with a dot, as tempdir names do.
        let tmp = tempfile::Builder::new().prefix("sandfs-git").tempdir().unwrap();
        let remote = tmp.path().join("remote");
        std::fs::create_dir(&remote).unwrap();
        run(&remote, &["init", "-q", "-b", "main"]);
        run(&remote, &["config", "uploadpack.allowFilter", "true"]);
        std::fs::write(remote.join("a.txt"), "one").unwrap();
        run(&remote, &["add", "a.txt"]);
        run(&remote, &["commit", "-q", "-m", "first"]);
        let first = run(&remote, &["rev-parse", "HEAD"]);
        std::fs::write(remote.join("a.txt"), "two").unwrap();
        run(&remote, &["commit", "-q", "-am", "second"]);
        let second = run(&remote, &["rev-parse", "HEAD"]);
        std::fs::write(remote.join("a.txt"), "three").unwrap();
        run(&remote, &["commit", "-q", "-am", "third"]);
        let third = run(&remote, &["rev-parse", "HEAD"]);
        let url = format!("file://{}", remote.display());
        let repos = tmp.path().join("repos");

        let (repo, commit) = resolve(&repos, &url).unwrap();
        assert_eq!(commit, third);
        // A URL that failed doesn't stick to the local copy, and a URL that fails doesn't
        // replace one that works. `tmp:sandfs-gitX/remote`, an ssh host that doesn't exist,
        // shares the local copy of `file:///tmp/sandfs-gitX/remote`.
        run(&repo, &["remote", "set-url", "origin", "file:///nowhere"]);
        assert_eq!(resolve(&repos, &format!("{url}@main")).unwrap().1, third);
        assert_eq!(run(&repo, &["config", "remote.origin.url"]), url);
        let unreachable = remote.to_str().unwrap().trim_start_matches('/').replacen('/', ":", 1);
        assert_eq!(repos.join(cache_path(&unreachable).unwrap()), repo);
        assert!(fetch_commit(&repos, &unreachable, "main").is_err());
        assert_eq!(run(&repo, &["config", "remote.origin.url"]), url);
        // The same holds for a commit mounted before, which needs no fetch.
        assert!(fetch_commit(&repos, &unreachable, &third).is_ok());
        assert_eq!(run(&repo, &["config", "remote.origin.url"]), url);
        run(&repo, &["remote", "set-url", "origin", "file:///nowhere"]);
        assert_eq!(resolve(&repos, &format!("{url}@{third}")).unwrap().1, third);
        assert_eq!(run(&repo, &["config", "remote.origin.url"]), url);
        // One commit and its listings: no history, and no file contents until read.
        assert_eq!(run(&repo, &["rev-list", "--count", "--all"]), "1");
        let objects = run(&repo, &["rev-list", "--objects", "--missing=print", &commit]);
        assert!(objects.lines().any(|line| line.starts_with('?')), "{objects}");

        // Older commits, branches and tags are fetched the same way when asked for, still
        // without the history behind them.
        assert_eq!(resolve(&repos, &format!("{url}@{second}")).unwrap().1, second);
        let commits = run(&repo, &["cat-file", "--batch-all-objects", "--batch-check=%(objecttype)"]);
        assert_eq!(commits.lines().filter(|kind| *kind == "commit").count(), 2, "{commits}");
        assert_eq!(resolve(&repos, &format!("{url}@main")).unwrap().1, third);
        let mut ws = sandfs_core::Workspace::new();
        ws.mount("/old", open(&repo, &second).unwrap()).unwrap();
        assert_eq!(&ws.read("/old/a.txt").unwrap()[..], b"two");

        // A commit id already fetched mounts with no request, so it works offline.
        std::fs::remove_dir_all(&remote).unwrap();
        assert_eq!(resolve(&repos, &format!("{url}@{second}")).unwrap().1, second);
        let mut ws = sandfs_core::Workspace::new();
        ws.mount("/old", open(&repo, &second).unwrap()).unwrap();
        assert_eq!(&ws.read("/old/a.txt").unwrap()[..], b"two");
        assert!(resolve(&repos, &format!("{url}@{first}")).is_err());
        // A name needs the server, so it fails offline instead of mounting something stale.
        let err = format!("{:#}", resolve(&repos, &format!("{url}@main")).unwrap_err());
        assert!(err.contains("only a full commit id that was mounted before mounts"), "{err}");
    }

    #[test]
    fn concurrent_mounts_of_one_remote_wait_for_each_other() {
        let tmp = tempfile::Builder::new().prefix("sandfs-git").tempdir().unwrap();
        let remote = tmp.path().join("remote");
        std::fs::create_dir(&remote).unwrap();
        run(&remote, &["init", "-q", "-b", "main"]);
        run(&remote, &["config", "uploadpack.allowFilter", "true"]);
        let commits: Vec<String> = (0..8)
            .map(|i| {
                std::fs::write(remote.join("a.txt"), i.to_string()).unwrap();
                run(&remote, &["add", "a.txt"]);
                run(&remote, &["commit", "-q", "-m", &i.to_string()]);
                run(&remote, &["rev-parse", "HEAD"])
            })
            .collect();
        let url = format!("file://{}", remote.display());
        let repos = tmp.path().join("repos");

        resolve(&repos, &url).unwrap();
        // Every fetch rewrites the local copy's shallow list, so unserialized ones collide.
        std::thread::scope(|scope| {
            let mounts: Vec<_> = commits
                .iter()
                .map(|commit| {
                    let (repos, spec) = (&repos, format!("{url}@{commit}"));
                    scope.spawn(move || resolve(repos, &spec).map(|(_, id)| id))
                })
                .collect();
            for (mount, commit) in mounts.into_iter().zip(&commits) {
                assert_eq!(&mount.join().unwrap().unwrap(), commit);
            }
        });
    }

    #[test]
    fn remote_repos_cache_by_host_and_path() {
        assert_eq!(
            cache_path("https://github.com/lydakis/sandfs").unwrap(),
            Path::new("github.com/lydakis/sandfs.git")
        );
        assert_eq!(cache_path("git@github.com:o/r.git").unwrap(), Path::new("github.com/o/r.git"));
        assert!(cache_path("https://host/../x").is_err());
        assert!(is_url("https://github.com/o/r") && is_url("git@github.com:o/r") && is_url("github.com:o/r"));
        assert!(!is_url("/home/me/repo") && !is_url("~/src/repo") && !is_url("./a:b"));
    }
}
