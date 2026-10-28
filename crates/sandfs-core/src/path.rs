//! Normalized absolute paths inside a workspace.

use std::borrow::Borrow;
use std::fmt;
use std::sync::Arc;

use crate::error::{Error, Result};

/// A normalized absolute path such as `/repo/src/lib.rs`; the root is `/`.
///
/// Parsing collapses repeated slashes and `.` components and resolves `..`; a `..`
/// that would climb above the root is rejected rather than clamped, so a caller can
/// never address something outside the workspace by accident.
///
/// A path never changes once made, so a copy shares its bytes: a mount's table of
/// files and the path each file is known by hold one string between them.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VPath(Arc<str>);

impl VPath {
    /// The root path `/`.
    pub fn root() -> Self {
        VPath("/".into())
    }

    /// Parse and normalize an absolute path.
    pub fn parse(raw: &str) -> Result<Self> {
        if !raw.starts_with('/') {
            return Err(Error::InvalidPath(raw.to_owned()));
        }
        // Most paths arrive already normalized; keep them as they are.
        let normal = |part: &str| !part.is_empty() && part != "." && part != ".." && !part.contains('\0');
        if raw == "/" || raw.split('/').skip(1).all(normal) {
            return Ok(VPath(raw.into()));
        }
        let mut parts: Vec<&str> = Vec::new();
        for part in raw.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    if parts.pop().is_none() {
                        return Err(Error::InvalidPath(raw.to_owned()));
                    }
                }
                other => {
                    if other.contains('\0') {
                        return Err(Error::InvalidPath(raw.to_owned()));
                    }
                    parts.push(other);
                }
            }
        }
        Ok(VPath(format!("/{}", parts.join("/")).into()))
    }

    /// The path as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True for `/`.
    pub fn is_root(&self) -> bool {
        &*self.0 == "/"
    }

    /// The parent directory, or `None` for the root.
    pub fn parent(&self) -> Option<VPath> {
        if self.is_root() {
            return None;
        }
        let idx = self.0.rfind('/').unwrap_or(0);
        Some(if idx == 0 { VPath::root() } else { VPath(self.0[..idx].into()) })
    }

    /// The last component, or `None` for the root.
    pub fn name(&self) -> Option<&str> {
        if self.is_root() { None } else { self.0.rsplit('/').next() }
    }

    /// Append one component. `name` must not be empty or contain `/`.
    pub fn join(&self, name: &str) -> Result<VPath> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(Error::InvalidPath(format!("{}/{}", self.0, name)));
        }
        Ok(if self.is_root() { VPath(format!("/{name}").into()) } else { VPath(format!("{}/{name}", self.0).into()) })
    }

    /// The components after the root, in order.
    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|part| !part.is_empty())
    }

    /// Number of components (0 for the root).
    pub fn depth(&self) -> usize {
        self.components().count()
    }

    /// True if `self` equals `prefix` or lies below it, compared component by component.
    pub fn starts_with(&self, prefix: &VPath) -> bool {
        prefix.is_root()
            || self.0 == prefix.0
            || (self.0.starts_with(&*prefix.0) && self.0.as_bytes().get(prefix.0.len()) == Some(&b'/'))
    }

    /// The components of `self` below `prefix`, if `self` starts with it.
    pub fn strip_prefix<'a>(&'a self, prefix: &VPath) -> Option<Vec<&'a str>> {
        if !self.starts_with(prefix) {
            return None;
        }
        Some(self.components().skip(prefix.depth()).collect())
    }

    /// Every proper ancestor from the root down, excluding `self`.
    pub fn ancestors(&self) -> Vec<VPath> {
        let mut out = Vec::new();
        let mut current = self.parent();
        while let Some(path) = current {
            current = path.parent();
            out.push(path);
        }
        out.reverse();
        out
    }

    /// Every proper ancestor from the root down, as slices of this path, so walking
    /// them allocates nothing.
    pub(crate) fn ancestor_strs(&self) -> impl Iterator<Item = &str> {
        let path = &*self.0;
        let root = (!self.is_root()).then_some("/");
        // Each '/' after the leading one ends an ancestor.
        root.into_iter().chain(path.match_indices('/').skip(1).map(move |(idx, _)| &path[..idx]))
    }

    /// The smallest string that sorts after every descendant of `self`, for range scans.
    pub(crate) fn descendants_end(&self) -> VPath {
        if self.is_root() {
            // '0' is the byte after '/', so it bounds every absolute path.
            VPath("0".into())
        } else {
            VPath(format!("{}0", self.0).into())
        }
    }

    /// The smallest string that sorts before every descendant of `self`.
    pub(crate) fn descendants_start(&self) -> VPath {
        VPath(if self.is_root() { "/".into() } else { format!("{}/", self.0).into() })
    }
}

/// Lets maps keyed by paths be searched with a `&str`, without building a `VPath`.
/// Paths compare and hash exactly like their strings.
impl Borrow<str> for VPath {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for VPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VPath({:?})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_normalizes() {
        assert_eq!(VPath::parse("//a/./b/../c/").unwrap().as_str(), "/a/c");
        assert_eq!(VPath::parse("/").unwrap().as_str(), "/");
        assert!(VPath::parse("/..").is_err());
        assert!(VPath::parse("relative").is_err());
        assert_eq!(VPath::parse("/a/b").unwrap().as_str(), "/a/b");
        assert_eq!(VPath::parse("/a/").unwrap().as_str(), "/a");
        assert_eq!(VPath::parse("/a/.").unwrap().as_str(), "/a");
        assert!(VPath::parse("/a\0b").is_err());
    }

    #[test]
    fn parent_name_and_prefix() {
        let path = VPath::parse("/repo/src/lib.rs").unwrap();
        assert_eq!(path.parent().unwrap().as_str(), "/repo/src");
        assert_eq!(path.name(), Some("lib.rs"));
        assert!(path.starts_with(&VPath::parse("/repo").unwrap()));
        assert!(!path.starts_with(&VPath::parse("/rep").unwrap()));
        assert_eq!(path.strip_prefix(&VPath::parse("/repo").unwrap()).unwrap(), ["src", "lib.rs"]);
        assert_eq!(path.ancestors().iter().map(VPath::as_str).collect::<Vec<_>>(), ["/", "/repo", "/repo/src"]);
        assert_eq!(path.ancestor_strs().collect::<Vec<_>>(), ["/", "/repo", "/repo/src"]);
        assert_eq!(VPath::parse("/repo").unwrap().ancestor_strs().collect::<Vec<_>>(), ["/"]);
        assert_eq!(VPath::root().ancestor_strs().count(), 0);
    }
}
