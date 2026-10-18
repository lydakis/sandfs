//! Git commits as sources: `git:<path or url>[@rev]`.
//!
//! Everything git-specific in the CLI lives here, so the rest of it only knows host
//! folders. Remote repositories become bare partial clones under `<home>/repos`, so
//! file contents are fetched only when a directory or file is first read.

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
    let repo = if is_url(location) {
        clone_or_fetch(repos, location)?
    } else {
        expand(location).canonicalize().with_context(|| format!("no repository at {location}"))?
    };
    let rev = rev.as_deref().unwrap_or("HEAD");
    if rev.starts_with('-') {
        bail!("invalid revision {rev:?}");
    }
    let commit = git(&repo, &["rev-parse", "--verify", "--end-of-options", &format!("{rev}^{{commit}}")])
        .with_context(|| format!("{rev:?} is not a commit in {}", repo.display()))?;
    Ok((repo, commit))
}

/// The source for `commit` of `repo`.
pub fn open(repo: &Path, commit: &str) -> Result<Arc<dyn Source>> {
    // Sizes let FUSE answer `stat` without reading whole files.
    Ok(Arc::new(GitSource::open(repo, commit)?.with_sizes()?))
}

/// The local partial clone of `url`, created on first use and fetched after.
fn clone_or_fetch(repos: &Path, url: &str) -> Result<PathBuf> {
    let dest = repos.join(cache_path(url)?);
    if dest.join("HEAD").exists() {
        // A stale clone still mounts, so a failed fetch (offline, say) only warns.
        let refs = ["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"];
        if let Err(err) = git(&dest, &[&["fetch", "-q", "--prune", "origin"][..], &refs[..]].concat()) {
            eprintln!("sandfs: could not update {url}, using the local copy: {err:#}");
        }
        return Ok(dest);
    }
    let parent = dest.parent().context("bad cache path")?;
    std::fs::create_dir_all(parent)?;
    // Clone beside the destination and rename, so an interrupted clone leaves nothing behind.
    let tmp = parent.join(format!(
        ".{}.tmp-{}",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("repo"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    eprintln!("sandfs: cloning {url} (file contents are fetched when first read)");
    let tmp_str = tmp.to_str().context("non-UTF-8 home directory")?;
    let status = Command::new("git")
        .args(["clone", "-q", "--bare", "--filter=blob:none", "--", url, tmp_str])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .status()
        .context("running git clone")?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("git clone {url} failed");
    }
    if let Err(err) = std::fs::rename(&tmp, &dest) {
        let _ = std::fs::remove_dir_all(&tmp);
        // Another mount of the same URL may have finished its clone first.
        if !dest.join("HEAD").exists() {
            return Err(err).context("moving the clone into place");
        }
    }
    Ok(dest)
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
    // an https user is followed by a host with no path before it.
    let user_of_url = location.split_once("://").is_some_and(|(_, rest)| !rest.contains('/'));
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
        assert_eq!(split("/srv/repo@main@{2.days.ago}"), ("/srv/repo".into(), Some("main@{2.days.ago}".into())));
        assert!(resolve(Path::new("/nonexistent"), "repo@").is_err());
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
