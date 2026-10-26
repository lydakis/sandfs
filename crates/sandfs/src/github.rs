//! File sizes from the service hosting a repository, which a partial clone can't tell
//! without downloading each file.
//!
//! A fresh git mount has every folder listing but no file contents, and git keeps a
//! file's size inside its contents, so a `stat` meant downloading the file. On macOS,
//! where a listing also says what each entry is only alongside its size, a `find` or
//! git's first `git status` downloaded much of the repository. GitHub lists a tree's
//! files with their sizes in one API request, so a mount from github.com asks for them
//! while it fetches the commit, keeps them in the local copy (`sandfs-sizes/<tree>`),
//! and its listings carry every size from the start. Other hosts, and a request that
//! fails, leave sizes to be found by downloading, as before.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use sandfs_core::SourceKey;

/// The sizes kept for `tree` in the local copy `repo`, by blob id; empty if none are.
pub fn known_sizes(repo: &Path, tree: &str) -> HashMap<SourceKey, u64> {
    let Ok(text) = std::fs::read_to_string(sizes_path(repo, tree)) else { return HashMap::new() };
    let entry = |line: &str| {
        let (id, size) = line.split_once(' ')?;
        Some((id.into(), size.parse().ok()?))
    };
    text.lines().filter_map(entry).collect()
}

/// Ask the host of `url` for the sizes of the files of `rev` (a commit or tree id, or a
/// plain ref name). Only github.com answers. Returns the commit or tree `rev` named,
/// as the host resolved it, which tells which tree the sizes belong to, and the sizes
/// by blob id.
pub fn fetch_sizes(url: &str, rev: &str) -> Option<(String, Vec<(String, u64)>)> {
    let (owner, name) = github_repo(url)?;
    if !plain_rev(rev) {
        return None;
    }
    let api = format!("https://api.github.com/repos/{owner}/{name}/git/trees/{rev}?recursive=1");
    parse_tree(&get(&api).ok()?).ok()
}

/// Keep `sizes` for `tree` in the local copy `repo`.
pub fn keep_sizes(repo: &Path, tree: &str, sizes: &[(String, u64)]) -> Result<()> {
    let path = sizes_path(repo, tree);
    let mut text = String::with_capacity(sizes.len() * 48);
    for (id, size) in sizes {
        text.push_str(&format!("{id} {size}\n"));
    }
    // Written aside and renamed, so a mount never reads half a file.
    let dir = path.parent().context("no folder for sizes")?;
    std::fs::create_dir_all(dir)?;
    let partial = dir.join(format!(".{tree}.{}", std::process::id()));
    std::fs::write(&partial, text)?;
    std::fs::rename(&partial, &path)?;
    Ok(())
}

/// Whether sizes are kept for `tree` in `repo`.
pub fn has_sizes(repo: &Path, tree: &str) -> bool {
    sizes_path(repo, tree).exists()
}

/// Whether `rev` can go in an API request's path as it is: a commit id or a plain
/// branch or tag name, unlike `feature/x` or `main~3`.
pub fn plain_rev(rev: &str) -> bool {
    !rev.is_empty() && !rev.starts_with('.') && rev.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

fn sizes_path(repo: &Path, tree: &str) -> PathBuf {
    repo.join("sandfs-sizes").join(tree)
}

/// `owner` and `repo` of a github.com URL: https, ssh, or scp-style `git@github.com:`.
fn github_repo(url: &str) -> Option<(String, String)> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
    let path = rest.strip_prefix("github.com/").or_else(|| rest.strip_prefix("github.com:"))?;
    let mut parts = path.trim_end_matches('/').trim_end_matches(".git").split('/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let valid = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    (parts.next().is_none() && valid(owner) && valid(name)).then(|| (owner.to_owned(), name.to_owned()))
}

/// GET `url` with curl, with a GitHub token when one is at hand (`GH_TOKEN`,
/// `GITHUB_TOKEN`, or the GitHub CLI's), which raises the limit of 60 requests an hour.
/// The token goes to curl on its standard input, never on its command line.
fn get(url: &str) -> Result<Vec<u8>> {
    let mut config = String::from("silent\nshow-error\nfail\nlocation\nmax-time = 10\n");
    config.push_str("header = \"Accept: application/vnd.github+json\"\n");
    if let Some(token) = token() {
        config.push_str(&format!("header = \"Authorization: Bearer {token}\"\n"));
    }
    let mut child = Command::new("curl")
        .args(["--config", "-", "--url", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running curl")?;
    child.stdin.take().context("curl's input")?.write_all(config.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!("curl {url}: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output.stdout)
}

fn token() -> Option<String> {
    let from_env = ["GH_TOKEN", "GITHUB_TOKEN"].into_iter().find_map(|name| std::env::var(name).ok());
    let token = from_env.or_else(|| {
        let output = Command::new("gh").args(["auth", "token"]).stderr(Stdio::null()).output().ok()?;
        output.status.success().then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    })?;
    let token = token.trim();
    // Anything else couldn't be a token, and would break the header.
    (!token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then(|| token.to_owned())
}

/// The id the request named (a commit's for a commit or a ref, else the tree's) and the
/// files' sizes from a GitHub tree response. A response cut short
/// for a huge tree (`truncated`) still has sizes for the files it lists.
fn parse_tree(body: &[u8]) -> Result<(String, Vec<(String, u64)>)> {
    let value: serde_json::Value = serde_json::from_slice(body)?;
    let tree = value["sha"].as_str().filter(|id| is_object_id(id)).context("no id")?.to_owned();
    let entries = value["tree"].as_array().context("no tree entries")?;
    let sizes = entries.iter().filter(|entry| entry["type"] == "blob").filter_map(|entry| {
        let id = entry["sha"].as_str().filter(|id| is_object_id(id))?;
        Some((id.to_owned(), entry["size"].as_u64()?))
    });
    Ok((tree, sizes.collect()))
}

fn is_object_id(id: &str) -> bool {
    matches!(id.len(), 40 | 64) && id.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_urls_name_their_repository() {
        let repo = |url| github_repo(url).map(|(owner, name)| format!("{owner}/{name}"));
        assert_eq!(repo("https://github.com/microsoft/vscode").as_deref(), Some("microsoft/vscode"));
        assert_eq!(repo("https://github.com/microsoft/vscode.git/").as_deref(), Some("microsoft/vscode"));
        assert_eq!(repo("git@github.com:lydakis/sandfs.git").as_deref(), Some("lydakis/sandfs"));
        assert_eq!(repo("ssh://git@github.com/lydakis/sandfs").as_deref(), Some("lydakis/sandfs"));
        assert_eq!(repo("https://gitlab.com/a/b"), None);
        assert_eq!(repo("https://github.com/a"), None);
        assert_eq!(repo("https://github.com/a/b/c"), None);
        assert_eq!(repo("https://github.com/a/b?x=1"), None);
    }

    #[test]
    fn sizes_are_kept_by_tree_and_read_back() {
        let tree = "a".repeat(40);
        let (blob, other) = ("b".repeat(40), "c".repeat(40));
        let body = format!(
            r#"{{"sha":"{tree}","truncated":false,"tree":[
                {{"path":"README.md","type":"blob","sha":"{blob}","size":7}},
                {{"path":"src","type":"tree","sha":"{other}"}},
                {{"path":"vendor","type":"commit","sha":"{other}"}}]}}"#
        );
        let (id, sizes) = parse_tree(body.as_bytes()).unwrap();
        assert_eq!((id.as_str(), sizes), (tree.as_str(), vec![(blob.clone(), 7)]));
        assert!(parse_tree(br#"{"message":"Not Found"}"#).is_err());

        let repo = tempfile::tempdir().unwrap();
        assert!(known_sizes(repo.path(), &tree).is_empty());
        std::fs::create_dir_all(repo.path().join("sandfs-sizes")).unwrap();
        std::fs::write(sizes_path(repo.path(), &tree), format!("{blob} 7\nnonsense\n")).unwrap();
        assert_eq!(known_sizes(repo.path(), &tree).get(blob.as_str()), Some(&7));
    }
}
