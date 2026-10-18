//! The file tools: argument parsing, calls into the workspace, and text for the model.

use std::collections::BTreeSet;

use base64::prelude::{BASE64_STANDARD, Engine};
use globset::{GlobBuilder, GlobMatcher};
use regex::RegexBuilder;
use sandfs_core::{ChangeKind, DirEntry, Error, Kind, VPath, Workspace};
use serde_json::{Map, Value, json};

/// Results longer than this many bytes are cut, with a note saying how to see the rest.
pub const MAX_OUTPUT: usize = 64 * 1024;
const DEFAULT_READ_LINES: usize = 2000;
const MAX_READ_LINE_CHARS: usize = 2000;
const MAX_GREP_LINE_CHARS: usize = 500;
const MAX_ENTRIES: usize = 1000;
const DEFAULT_GREP_MATCHES: usize = 200;
const MAX_CONTEXT: u64 = 20;
const MAX_LIST_DEPTH: u64 = 20;

type ToolResult = Result<String, String>;

/// The workspace an MCP session works on.
pub(crate) struct Tools {
    pub(crate) workspace: Workspace,
}

impl Tools {
    pub(crate) fn new(workspace: Workspace) -> Self {
        Tools { workspace }
    }

    /// Run a tool; `None` means there is no tool with that name.
    pub(crate) fn call(&mut self, name: &str, arguments: &Value) -> Option<ToolResult> {
        let empty = Map::new();
        let args = match arguments {
            Value::Object(map) => Args(map),
            Value::Null => Args(&empty),
            _ => return Some(Err("arguments must be a JSON object".to_owned())),
        };
        let result = match name {
            "read" => self.read(&args),
            "list" => self.list(&args),
            "glob" => self.glob(&args),
            "grep" => self.grep(&args),
            "write" => self.write(&args),
            "edit" => self.edit(&args),
            "move" => self.rename(&args),
            "delete" => self.delete(&args),
            "changes" => self.changes(&args),
            _ => return None,
        };
        Some(result.map(cap).map_err(cap))
    }

    /// What the agent is told at `initialize`: the top-level layout and how to work.
    pub(crate) fn instructions(&self) -> String {
        let top: Vec<String> = match self.workspace.list("/") {
            Ok(entries) => entries.iter().map(|entry| format!("/{}{}", entry.name, suffix(entry.kind))).collect(),
            Err(_) => Vec::new(),
        };
        let layout = if top.is_empty() {
            "The workspace is empty.".to_owned()
        } else {
            format!("It contains {}.", top.join(", "))
        };
        format!(
            "These tools are your filesystem: a private copy-on-write workspace. {layout} Paths are absolute. \
             Mounted sources are never modified; every write stays in this workspace, and `changes` shows what you \
             changed."
        )
    }

    fn read(&self, args: &Args) -> ToolResult {
        let path = args.path("path")?;
        let offset = args.opt_u64("offset")?.unwrap_or(1).max(1) as usize;
        let limit = args.opt_u64("limit")?.map_or(DEFAULT_READ_LINES, |n| n as usize).max(1);
        let version = self.workspace.stat(&path).map_err(err)?.version;
        let bytes = self.workspace.read(&path).map_err(err)?;
        let Some(text) = as_text(&bytes) else {
            let kind = detect_type(&bytes);
            return Ok(format!("{path}: {kind}, {} bytes, version {version}; not text, so not shown", bytes.len()));
        };
        if text.is_empty() {
            return Ok(format!("{path}: empty file, version {version}"));
        }
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();
        if offset > total {
            return Err(format!("{path}: offset {offset} is past the end of the file ({total} lines)"));
        }
        let mut out = String::new();
        let mut last = offset - 1;
        for (idx, line) in lines.iter().enumerate().skip(offset - 1).take(limit) {
            let entry = format!("{:>6}\t{}\n", idx + 1, clip(line, MAX_READ_LINE_CHARS));
            // Leave room for the footer, but always show at least one line.
            if !out.is_empty() && out.len() + entry.len() > MAX_OUTPUT - 200 {
                break;
            }
            out.push_str(&entry);
            last = idx + 1;
        }
        out.push_str(&format!("[version {version} · lines {offset}-{last} of {total}"));
        if last < total {
            out.push_str(&format!(" · continue with offset={}", last + 1));
        }
        // Line endings are not shown, but exact-text edits must match them.
        if text.contains("\r\n") {
            out.push_str(" · CRLF line endings: use \\r\\n in edit text");
        }
        if !text.ends_with('\n') && last == total {
            out.push_str(" · no newline at end of file");
        }
        out.push(']');
        Ok(out)
    }

    fn list(&self, args: &Args) -> ToolResult {
        let path = args.opt_path("path")?;
        let depth = args.opt_u64("depth")?.unwrap_or(1).clamp(1, MAX_LIST_DEPTH) as usize;
        let mut lines = Vec::new();
        let mut truncated = false;
        walk(&self.workspace, &path, depth, &mut |_, rel, entry| {
            if lines.len() == MAX_ENTRIES {
                truncated = true;
                return false;
            }
            lines.push(match (entry.kind, entry.size) {
                (Kind::Dir, _) => format!("{rel}/"),
                (_, Some(size)) => format!("{rel} ({size} bytes)"),
                (_, None) => format!("{rel} (size unknown)"),
            });
            true
        })?;
        if lines.is_empty() {
            return Ok(format!("{path} is empty"));
        }
        let mut out = lines.join("\n");
        if truncated {
            out.push_str(&format!("\n[stopped at {MAX_ENTRIES} entries; list a subdirectory or use a smaller depth]"));
        }
        Ok(out)
    }

    fn glob(&self, args: &Args) -> ToolResult {
        let pattern = args.str("pattern")?;
        let root = args.opt_path("path")?;
        let matcher = glob_matcher(pattern)?;
        // An absolute pattern matches whole paths; a relative one matches below `path`.
        let absolute = pattern.starts_with('/');
        let mut found = Vec::new();
        let mut truncated = false;
        walk(&self.workspace, &root, usize::MAX, &mut |abs, rel, entry| {
            if entry.kind.is_dir() || !matcher.is_match(if absolute { abs } else { rel }) {
                return true;
            }
            if found.len() == MAX_ENTRIES {
                truncated = true;
                return false;
            }
            found.push(abs.to_owned());
            true
        })?;
        if found.is_empty() {
            return Ok(format!("No files match {pattern} under {root}"));
        }
        let mut out = found.join("\n");
        if truncated {
            out.push_str(&format!("\n[first {MAX_ENTRIES} matches; narrow the pattern or the path]"));
        }
        Ok(out)
    }

    fn grep(&self, args: &Args) -> ToolResult {
        let pattern = args.str("pattern")?;
        let root = args.opt_path("path")?;
        let filter = args.opt_str("glob")?.map(|glob| Ok::<_, String>((glob_matcher(glob)?, glob.contains('/'))));
        let filter = filter.transpose()?;
        let context = args.opt_u64("context")?.unwrap_or(0).min(MAX_CONTEXT) as usize;
        let max = args.opt_u64("max")?.map_or(DEFAULT_GREP_MATCHES, |n| n as usize).max(1);
        let regex = RegexBuilder::new(pattern)
            .case_insensitive(args.opt_bool("ignore_case")?)
            .build()
            .map_err(|e| format!("invalid regex: {e}"))?;

        let mut search = Search {
            regex: &regex,
            context,
            max,
            out: String::new(),
            matches: 0,
            files: 0,
            unreadable: 0,
            truncated: false,
        };
        if self.workspace.stat(&root).map_err(err)?.kind.is_dir() {
            walk(&self.workspace, &root, usize::MAX, &mut |abs, rel, entry| {
                if entry.kind.is_dir() {
                    return true;
                }
                // Like ripgrep, a glob without a slash matches the file name at any depth.
                if let Some((matcher, has_slash)) = &filter {
                    let name = if *has_slash { rel } else { rel.rsplit('/').next().unwrap_or(rel) };
                    if !matcher.is_match(name) {
                        return true;
                    }
                }
                search.file(&self.workspace, abs)
            })?;
        } else {
            search.file(&self.workspace, &root);
        }

        if search.matches == 0 {
            let unread = match search.unreadable {
                0 => String::new(),
                n => format!(" ({n} files could not be read)"),
            };
            return Ok(format!("No matches for {pattern} under {root}{unread}"));
        }
        let mut out = search.out;
        let files = if search.files == 1 { "file" } else { "files" };
        out.push_str(&format!("[{} matches in {} {files}", search.matches, search.files));
        if search.truncated {
            out.push_str("; output cut short, narrow the path or glob");
        } else if search.matches >= max {
            out.push_str(&format!("; stopped at max={max}"));
        }
        if search.unreadable > 0 {
            out.push_str(&format!("; {} files could not be read", search.unreadable));
        }
        out.push(']');
        Ok(out)
    }

    fn write(&mut self, args: &Args) -> ToolResult {
        let path = args.path("path")?;
        let content = args.str("content")?;
        let data = match args.opt_str("encoding")?.unwrap_or("text") {
            "text" => content.as_bytes().to_vec(),
            "base64" => {
                BASE64_STANDARD.decode(content.trim()).map_err(|e| format!("content is not valid base64: {e}"))?
            }
            other => return Err(format!("unknown encoding {other:?}; use text or base64")),
        };
        let size = data.len();
        let version = match args.opt_u64("expected_version")? {
            Some(expected) => self.workspace.write_versioned(&path, data, expected),
            None => self.workspace.write(&path, data),
        }
        .map_err(err)?;
        Ok(format!("Wrote {path} ({size} bytes), version {version}"))
    }

    fn edit(&mut self, args: &Args) -> ToolResult {
        let path = args.path("path")?;
        let edits = args.edits()?;
        let version = self.workspace.stat(&path).map_err(err)?.version;
        if let Some(expected) = args.opt_u64("expected_version")? {
            if expected != version {
                return Err(Error::VersionMismatch { path, expected, actual: version }.to_string());
            }
        }
        let bytes = self.workspace.read(&path).map_err(err)?;
        let mut text =
            as_text(&bytes).ok_or_else(|| format!("{path}: not a text file, so it cannot be edited"))?.to_owned();
        for edit in &edits {
            let count = text.matches(edit.old.as_str()).count();
            match (count, edit.replace_all) {
                (0, _) => return Err(format!("{path}: edit failed: text not found: {:?}", edit.old)),
                (1, _) | (_, true) => text = text.replace(edit.old.as_str(), &edit.new),
                (n, false) => {
                    return Err(format!("{path}: edit failed: text matches {n} times; add context or set replace_all"));
                }
            }
        }
        // The versioned write fails if anything changed the file since it was read.
        let version = self.workspace.write_versioned(&path, text, version).map_err(err)?;
        Ok(format!("Edited {path}, version {version}"))
    }

    fn rename(&mut self, args: &Args) -> ToolResult {
        let from = args.path("from")?;
        let to = args.path("to")?;
        self.workspace.rename(&from, &to).map_err(err)?;
        Ok(format!("Moved {from} to {to}"))
    }

    fn delete(&mut self, args: &Args) -> ToolResult {
        let path = args.path("path")?;
        self.workspace.remove(&path, args.opt_bool("recursive")?).map_err(err)?;
        Ok(format!("Deleted {path}"))
    }

    fn changes(&self, args: &Args) -> ToolResult {
        let scope = VPath::parse(&args.opt_path("path")?).map_err(err)?;
        let mut lines = Vec::new();
        for change in self.workspace.diff().map_err(err)?.iter().filter(|change| change.path.starts_with(&scope)) {
            let path = change.path.as_str();
            lines.push(match change.kind {
                ChangeKind::Deleted => format!("D {path}"),
                kind => {
                    let mark = if kind == ChangeKind::Added { 'A' } else { 'M' };
                    let size = self.workspace.stat(path).map_err(err)?.size;
                    format!("{mark} {path} ({size} bytes)")
                }
            });
        }
        if lines.is_empty() {
            return Ok(format!("No changes under {scope}"));
        }
        lines.push("[A added, M modified, D deleted; read a file to see its contents]".to_owned());
        Ok(lines.join("\n"))
    }
}

/// Grep state carried across the files of one search.
struct Search<'a> {
    regex: &'a regex::Regex,
    context: usize,
    max: usize,
    out: String,
    matches: usize,
    files: usize,
    unreadable: usize,
    truncated: bool,
}

impl Search<'_> {
    /// Search one file; returns false once the search should stop.
    fn file(&mut self, workspace: &Workspace, path: &str) -> bool {
        let regex = self.regex;
        let Ok(bytes) = workspace.read(path) else {
            self.unreadable += 1;
            return true;
        };
        let Some(text) = as_text(&bytes) else { return true };
        let lines: Vec<&str> = text.lines().collect();
        let hits: Vec<usize> =
            (0..lines.len()).filter(|&n| regex.is_match(lines[n])).take(self.max - self.matches).collect();
        if hits.is_empty() {
            return true;
        }
        // Merge each hit's context window with the previous one when they touch.
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for &hit in &hits {
            let (start, end) = (hit.saturating_sub(self.context), (hit + self.context).min(lines.len() - 1));
            match ranges.last_mut() {
                Some(last) if start <= last.1 + 1 => last.1 = end,
                _ => ranges.push((start, end)),
            }
        }
        let hit_set: BTreeSet<usize> = hits.iter().copied().collect();
        self.files += 1;
        for (start, end) in ranges {
            if self.context > 0 && !self.out.is_empty() {
                self.out.push_str("--\n");
            }
            for (n, text) in lines.iter().enumerate().take(end + 1).skip(start) {
                let sep = if hit_set.contains(&n) { ':' } else { '-' };
                let line = format!("{path}{sep}{}{sep}{}\n", n + 1, clip(text, MAX_GREP_LINE_CHARS));
                if self.out.len() + line.len() > MAX_OUTPUT - 200 {
                    self.truncated = true;
                    self.matches += hit_set.range(..n).count();
                    return false;
                }
                self.out.push_str(&line);
            }
        }
        self.matches += hits.len();
        self.matches < self.max
    }
}

/// One exact-text replacement for `edit`.
struct TextEdit {
    old: String,
    new: String,
    replace_all: bool,
}

/// Arguments of one tool call.
struct Args<'a>(&'a Map<String, Value>);

impl Args<'_> {
    fn opt_str(&self, key: &str) -> Result<Option<&str>, String> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value)),
            Some(_) => Err(format!("`{key}` must be a string")),
        }
    }

    fn str(&self, key: &str) -> Result<&str, String> {
        self.opt_str(key)?.ok_or_else(|| format!("missing required argument `{key}`"))
    }

    /// Numbers are also accepted as strings, since models sometimes quote them.
    fn opt_u64(&self, key: &str) -> Result<Option<u64>, String> {
        let invalid = || format!("`{key}` must be a non-negative integer");
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Number(n)) => n.as_u64().map(Some).ok_or_else(invalid),
            Some(Value::String(s)) => s.trim().parse().map(Some).map_err(|_| invalid()),
            Some(_) => Err(invalid()),
        }
    }

    fn opt_bool(&self, key: &str) -> Result<bool, String> {
        match self.0.get(key) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(Value::String(s)) if s == "true" || s == "false" => Ok(s == "true"),
            Some(_) => Err(format!("`{key}` must be true or false")),
        }
    }

    fn path(&self, key: &str) -> Result<String, String> {
        let raw = self.str(key)?;
        if !raw.starts_with('/') {
            return Err(format!("{raw}: paths are absolute; start them with /"));
        }
        Ok(raw.to_owned())
    }

    fn opt_path(&self, key: &str) -> Result<String, String> {
        if self.opt_str(key)?.is_none() { Ok("/".to_owned()) } else { self.path(key) }
    }

    fn edits(&self) -> Result<Vec<TextEdit>, String> {
        let Some(Value::Array(items)) = self.0.get("edits") else {
            return Err("`edits` must be a list of {old, new, replace_all?} objects".to_owned());
        };
        if items.is_empty() {
            return Err("`edits` is empty".to_owned());
        }
        items
            .iter()
            .map(|item| {
                let Value::Object(map) = item else { return Err("each edit must be an object".to_owned()) };
                let edit = Args(map);
                let old = edit.str("old")?;
                if old.is_empty() {
                    return Err("an edit's `old` text is empty".to_owned());
                }
                Ok(TextEdit {
                    old: old.to_owned(),
                    new: edit.str("new")?.to_owned(),
                    replace_all: edit.opt_bool("replace_all")?,
                })
            })
            .collect()
    }
}

/// Visit everything below `root` depth-first in name order, up to `max_depth` levels.
/// The visitor gets the absolute path, the path relative to `root` and the entry, and
/// returns false to stop the walk.
fn walk(
    workspace: &Workspace,
    root: &str,
    max_depth: usize,
    visit: &mut dyn FnMut(&str, &str, &DirEntry) -> bool,
) -> Result<(), String> {
    fn walk_dir(
        workspace: &Workspace,
        dir: &str,
        rel: &str,
        depth_left: usize,
        visit: &mut dyn FnMut(&str, &str, &DirEntry) -> bool,
    ) -> Result<bool, String> {
        for entry in workspace.list(dir).map_err(err)? {
            let abs = if dir == "/" { format!("/{}", entry.name) } else { format!("{dir}/{}", entry.name) };
            let child_rel = if rel.is_empty() { entry.name.clone() } else { format!("{rel}/{}", entry.name) };
            if !visit(&abs, &child_rel, &entry) {
                return Ok(false);
            }
            let descend = entry.kind.is_dir() && depth_left > 1;
            if descend && !walk_dir(workspace, &abs, &child_rel, depth_left - 1, visit)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
    let root = VPath::parse(root).map_err(err)?;
    walk_dir(workspace, root.as_str(), "", max_depth, visit).map(|_| ())
}

fn glob_matcher(pattern: &str) -> Result<GlobMatcher, String> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|e| format!("invalid glob {pattern:?}: {e}"))
}

/// A short description of a non-text file, sniffed from its first bytes.
fn detect_type(bytes: &[u8]) -> &'static str {
    const SIGNATURES: &[(&[u8], &str)] = &[
        (b"\x89PNG\r\n\x1a\n", "PNG image"),
        (b"\xFF\xD8\xFF", "JPEG image"),
        (b"GIF8", "GIF image"),
        (b"%PDF-", "PDF document"),
        (b"PK\x03\x04", "ZIP archive (also docx, xlsx, pptx)"),
        (b"\x1F\x8B", "gzip archive"),
        (b"SQLite format 3\0", "SQLite database"),
        (b"PAR1", "Parquet file"),
    ];
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return "WebP image";
    }
    SIGNATURES.iter().find(|(magic, _)| bytes.starts_with(magic)).map_or("binary file", |(_, name)| name)
}

/// Text if the bytes are UTF-8 without NUL bytes; anything else is treated as binary.
fn as_text(bytes: &[u8]) -> Option<&str> {
    if bytes.contains(&0) { None } else { std::str::from_utf8(bytes).ok() }
}

fn clip(line: &str, max_chars: usize) -> String {
    match line.char_indices().nth(max_chars) {
        Some((idx, _)) => format!("{}… [line cut at {max_chars} characters]", &line[..idx]),
        None => line.to_owned(),
    }
}

/// Cut a result at [`MAX_OUTPUT`] bytes on a line boundary, saying so.
fn cap(mut text: String) -> String {
    if text.len() <= MAX_OUTPUT {
        return text;
    }
    let mut cut = MAX_OUTPUT - 100;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    if let Some(newline) = text[..cut].rfind('\n') {
        cut = newline + 1;
    }
    text.truncate(cut);
    text.push_str("[output cut at 64 KiB; narrow the request, e.g. with a path, offset or limit]");
    text
}

fn suffix(kind: Kind) -> &'static str {
    if kind.is_dir() { "/" } else { "" }
}

fn err(error: Error) -> String {
    error.to_string()
}

/// The tool list sent in `tools/list`.
pub(crate) fn definitions() -> Vec<Value> {
    let path = |what: &str| json!({ "type": "string", "description": what });
    let read_only = json!({ "readOnlyHint": true });
    vec![
        json!({
            "name": "read",
            "description": "Read a file, with its version for `write`/`edit`. Text files come back with line numbers \
                (like `cat -n`), up to 2000 lines; use offset and limit for more. Other files report their \
                detected type and size, not their bytes.",
            "inputSchema": { "type": "object", "properties": {
                "path": path("Absolute path of the file."),
                "offset": { "type": "integer", "minimum": 1, "description": "First line to show (1-based)." },
                "limit": { "type": "integer", "minimum": 1, "description": "Number of lines to show." },
            }, "required": ["path"] },
            "annotations": read_only,
        }),
        json!({
            "name": "list",
            "description": "List a directory. Directories end in `/`; files show their size when known.",
            "inputSchema": { "type": "object", "properties": {
                "path": path("Absolute directory path. Defaults to /."),
                "depth": { "type": "integer", "minimum": 1, "description": "Levels to descend (default 1)." },
            } },
            "annotations": read_only,
        }),
        json!({
            "name": "glob",
            "description": "Find files by glob pattern, such as `**/*.csv` or `reports/*.md`. `*` stays within \
                one directory and `**` crosses directories. Relative patterns match below `path`; \
                absolute ones match whole paths.",
            "inputSchema": { "type": "object", "properties": {
                "pattern": { "type": "string", "description": "Glob pattern." },
                "path": path("Directory to search. Defaults to /."),
            }, "required": ["pattern"] },
            "annotations": read_only,
        }),
        json!({
            "name": "grep",
            "description": "Search text files with a regular expression (ripgrep syntax). Prints `path:line:text` \
                for matches and `path-line-text` for context lines. Files that are not text are skipped.",
            "inputSchema": { "type": "object", "properties": {
                "pattern": { "type": "string", "description": "Regular expression." },
                "path": path("File or directory to search. Defaults to /."),
                "glob": { "type": "string", "description": "Only search matching files, e.g. `*.json` (file name) or `logs/**/*.txt` (path below `path`)." },
                "ignore_case": { "type": "boolean", "description": "Case-insensitive search." },
                "context": { "type": "integer", "minimum": 0, "description": "Lines of context around each match." },
                "max": { "type": "integer", "minimum": 1, "description": "Stop after this many matches (default 200)." },
            }, "required": ["pattern"] },
            "annotations": read_only,
        }),
        json!({
            "name": "write",
            "description": "Create or overwrite a file, creating parent directories. Content is text, or base64 for \
                binary files. Pass the version from `read` as expected_version to fail instead of \
                overwriting a change you have not seen (a new file, or a mounted file not yet changed, \
                has version 0).",
            "inputSchema": { "type": "object", "properties": {
                "path": path("Absolute path of the file."),
                "content": { "type": "string", "description": "The full new contents." },
                "encoding": { "type": "string", "enum": ["text", "base64"], "description": "How content is encoded (default text)." },
                "expected_version": { "type": "integer", "minimum": 0, "description": "Version the file must have." },
            }, "required": ["path", "content"] },
        }),
        json!({
            "name": "edit",
            "description": "Replace exact text in a text file. Each `old` must match exactly once unless replace_all \
                is set; edits apply in order and either all apply or none do. Returns the new version.",
            "inputSchema": { "type": "object", "properties": {
                "path": path("Absolute path of the file."),
                "edits": { "type": "array", "minItems": 1, "items": { "type": "object", "properties": {
                    "old": { "type": "string", "description": "Exact text to replace, including whitespace." },
                    "new": { "type": "string", "description": "Replacement text." },
                    "replace_all": { "type": "boolean", "description": "Replace every occurrence." },
                }, "required": ["old", "new"] } },
                "expected_version": { "type": "integer", "minimum": 0, "description": "Version the file must have." },
            }, "required": ["path", "edits"] },
        }),
        json!({
            "name": "move",
            "description": "Move or rename a file or directory. The destination must not exist.",
            "inputSchema": { "type": "object", "properties": {
                "from": path("Absolute source path."),
                "to": path("Absolute destination path."),
            }, "required": ["from", "to"] },
        }),
        json!({
            "name": "delete",
            "description": "Delete a file or an empty directory; set recursive to delete a directory tree.",
            "inputSchema": { "type": "object", "properties": {
                "path": path("Absolute path."),
                "recursive": { "type": "boolean", "description": "Delete a directory and everything in it." },
            }, "required": ["path"] },
        }),
        json!({
            "name": "changes",
            "description": "List the files changed against the mounted sources: added (A), modified (M) or deleted \
                (D), with their size. Read a file to see its new contents.",
            "inputSchema": { "type": "object", "properties": {
                "path": path("Only show changes at or below this path."),
            } },
            "annotations": read_only,
        }),
    ]
}
