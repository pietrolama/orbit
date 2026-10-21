//! Limited, safe extraction of explicit local references from Markdown and plain-text files.
//!
//! Scope of the first version:
//! * Markdown (`.md`, `.markdown`, `.mdown`, `.mkd`): inline links `[t](target)`, images
//!   `![a](target)`, reference definitions `[id]: target`, autolinks `<file:///...>`. Fenced code
//!   blocks are skipped.
//! * Plain text (`.txt`, `.text`): whitespace-separated tokens that look like explicit paths
//!   (`./x`, `../x`, `~/x`, `/x/y`).
//!
//! Safety limits: only files up to [`MAX_SOURCE_BYTES`] are read; a NUL byte in the first 8 KiB
//! marks the file as binary and stops parsing; remote URLs and non-`file:` schemes are ignored;
//! nothing is ever executed or fetched. Targets are resolved lexically (no symlink resolution)
//! and only become relations if they match a known present entity.

use crate::identity::parent_raw;
use crate::model::{EntityId, EntityState, EntryType, GraphNode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::MetadataExt;

/// Maximum size of a source file parsed for references.
pub const MAX_SOURCE_BYTES: u64 = 1024 * 1024;
const BINARY_SNIFF_BYTES: usize = 8 * 1024;
const MAX_REFERENCES_PER_FILE: usize = 500;

/// One reference found in a source file, already resolved to an absolute lexical path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedReference {
    #[serde(skip)]
    pub target_path_raw: Vec<u8>,
    pub target_display: String,
    pub line: u32,
    /// `markdown_link`, `markdown_image`, `markdown_definition`, `autolink` or `plain_path`.
    pub syntax: String,
}

/// Cached extraction result for one version of a source file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceCacheEntry {
    pub source_entity_id: EntityId,
    pub size_bytes: u64,
    pub mtime_sec: Option<i64>,
    pub mtime_nsec: Option<u32>,
    pub references: Vec<ExtractedReference>,
}

impl ReferenceCacheEntry {
    pub fn matches_version(&self, node: &GraphNode) -> bool {
        self.size_bytes == node.apparent_bytes
            && self.mtime_sec == node.mtime_sec
            && self.mtime_nsec == node.mtime_nsec
    }
}

/// Statistics of a reference extraction pass.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReferenceReport {
    pub source_files: usize,
    pub reused_from_cache: usize,
    pub parsed: usize,
    pub skipped_too_large: usize,
    pub skipped_binary: usize,
    pub read_errors: usize,
    pub references_found: usize,
}

/// Syntax family of a source file, decided by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceSyntax {
    Markdown,
    PlainText,
}

pub fn source_syntax(node: &GraphNode) -> Option<SourceSyntax> {
    if node.kind != EntryType::File {
        return None;
    }
    match node.extension().as_str() {
        "md" | "markdown" | "mdown" | "mkd" => Some(SourceSyntax::Markdown),
        "txt" | "text" => Some(SourceSyntax::PlainText),
        _ => None,
    }
}

/// Lexically normalises an absolute raw path: removes `.`, resolves `..`, collapses `//`.
pub fn normalize_lexically(path: &[u8]) -> Vec<u8> {
    let mut parts: Vec<&[u8]> = Vec::new();
    for comp in path.split(|&b| b == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let mut out = Vec::with_capacity(path.len());
    for p in parts {
        out.push(b'/');
        out.extend_from_slice(p);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

fn percent_decode(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = (bytes[i + 1] as char).to_digit(16);
            let l = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (h, l) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

fn has_scheme(target: &str) -> Option<&str> {
    let idx = target.find(':')?;
    let scheme = &target[..idx];
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if first.is_ascii_alphabetic()
        && scheme.len() >= 2
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    {
        Some(scheme)
    } else {
        None
    }
}

/// Resolves a raw reference target found in `source_path` into an absolute lexical path.
/// Returns `None` for remote URLs, other schemes, anchors-only links and empty targets.
pub fn resolve_target(source_path: &[u8], target: &str, home: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut t = target.trim();
    if t.starts_with('<') && t.ends_with('>') && t.len() >= 2 {
        t = &t[1..t.len() - 1];
    }
    if let Some(scheme) = has_scheme(t) {
        if !scheme.eq_ignore_ascii_case("file") {
            return None;
        }
        let rest = &t[scheme.len() + 1..];
        let rest = rest.strip_prefix("//")?;
        // Only local file URLs: empty host or localhost.
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        if !rest.starts_with('/') {
            return None;
        }
        t = rest;
    }
    let t = t.split(['#', '?']).next().unwrap_or("");
    if t.is_empty() {
        return None;
    }
    let decoded = percent_decode(t);
    let abs: Vec<u8> = if decoded.starts_with(b"/") {
        decoded
    } else if let Some(rest) = decoded.strip_prefix(b"~/") {
        let home = home?;
        let mut v = home.to_vec();
        v.push(b'/');
        v.extend_from_slice(rest);
        v
    } else {
        let base = parent_raw(source_path)?;
        let mut v = base.to_vec();
        v.push(b'/');
        v.extend_from_slice(&decoded);
        v
    };
    let norm = normalize_lexically(&abs);
    if norm == source_path {
        return None;
    }
    Some(norm)
}

fn push_ref(
    out: &mut Vec<ExtractedReference>,
    source_path: &[u8],
    target: &str,
    line: u32,
    syntax: &str,
    home: Option<&[u8]>,
) {
    if out.len() >= MAX_REFERENCES_PER_FILE {
        return;
    }
    if let Some(p) = resolve_target(source_path, target, home) {
        if !out.iter().any(|r| r.target_path_raw == p) {
            out.push(ExtractedReference {
                target_display: String::from_utf8_lossy(&p).into_owned(),
                target_path_raw: p,
                line,
                syntax: syntax.to_string(),
            });
        }
    }
}

/// Extracts the link target starting right after `](`. Handles `<...>` targets and optional titles.
fn link_target(rest: &str) -> Option<&str> {
    let rest = rest.trim_start();
    if let Some(r) = rest.strip_prefix('<') {
        let end = r.find('>')?;
        return Some(&r[..end]);
    }
    let mut depth = 0i32;
    for (i, c) in rest.char_indices() {
        match c {
            '(' => depth += 1,
            ')' if depth == 0 => return Some(&rest[..i]),
            ')' => depth -= 1,
            ' ' | '\t' if depth == 0 => return Some(&rest[..i]),
            _ => {}
        }
    }
    None
}

/// Parses Markdown text and returns resolved references.
pub fn extract_markdown(
    source_path: &[u8],
    text: &str,
    home: Option<&[u8]>,
) -> Vec<ExtractedReference> {
    let mut out = Vec::new();
    let mut in_fence: Option<&str> = None;
    for (lineno, line) in text.lines().enumerate() {
        let line_no = lineno as u32 + 1;
        let trimmed = line.trim_start();
        let fence = if trimmed.starts_with("```") {
            Some("```")
        } else if trimmed.starts_with("~~~") {
            Some("~~~")
        } else {
            None
        };
        if let Some(f) = fence {
            match in_fence {
                Some(open) if open == f => in_fence = None,
                None => in_fence = Some(f),
                _ => {}
            }
            continue;
        }
        if in_fence.is_some() {
            continue;
        }

        // Reference definition: `[id]: target "title"`
        if let Some(r) = trimmed.strip_prefix('[') {
            if let Some(close) = r.find("]:") {
                if !r[..close].contains(']') {
                    let target = r[close + 2..].split_whitespace().next().unwrap_or("");
                    push_ref(
                        &mut out,
                        source_path,
                        target,
                        line_no,
                        "markdown_definition",
                        home,
                    );
                    continue;
                }
            }
        }

        // Inline links and images.
        let mut search_from = 0;
        while let Some(pos) = line[search_from..].find("](") {
            let abs_pos = search_from + pos;
            let before = &line[..abs_pos];
            let is_image = before
                .rfind('[')
                .map(|open| open > 0 && line.as_bytes()[open - 1] == b'!')
                .unwrap_or(false);
            if let Some(target) = link_target(&line[abs_pos + 2..]) {
                let syntax = if is_image {
                    "markdown_image"
                } else {
                    "markdown_link"
                };
                push_ref(&mut out, source_path, target, line_no, syntax, home);
            }
            search_from = abs_pos + 2;
        }

        // Autolinks restricted to file URLs: `<file:///...>`
        let mut from = 0;
        while let Some(pos) = line[from..].find("<file:") {
            let start = from + pos + 1;
            if let Some(end) = line[start..].find('>') {
                push_ref(
                    &mut out,
                    source_path,
                    &line[start..start + end],
                    line_no,
                    "autolink",
                    home,
                );
                from = start + end;
            } else {
                break;
            }
        }
    }
    out
}

/// Parses plain text and returns tokens that look like explicit local paths.
pub fn extract_plain_text(
    source_path: &[u8],
    text: &str,
    home: Option<&[u8]>,
) -> Vec<ExtractedReference> {
    let mut out = Vec::new();
    for (lineno, line) in text.lines().enumerate() {
        for token in line.split_whitespace() {
            let tok = token.trim_matches(|c: char| {
                matches!(
                    c,
                    '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | ';' | '`'
                )
            });
            let tok = tok.trim_end_matches(['.', ':']);
            let looks_like_path = tok.starts_with("./")
                || tok.starts_with("../")
                || tok.starts_with("~/")
                || tok.starts_with("file://")
                || (tok.starts_with('/') && tok.len() > 1 && tok[1..].contains('/'));
            if looks_like_path {
                push_ref(
                    &mut out,
                    source_path,
                    tok,
                    lineno as u32 + 1,
                    "plain_path",
                    home,
                );
            }
        }
    }
    out
}

/// Reads and parses one source file within the safety limits.
fn parse_file(
    node: &GraphNode,
    syntax: SourceSyntax,
    home: Option<&[u8]>,
) -> Result<Vec<ExtractedReference>, &'static str> {
    if node.apparent_bytes > MAX_SOURCE_BYTES {
        return Err("too_large");
    }
    let path = crate::scanner::raw_bytes_to_path_buf(&node.path_raw);
    let meta = fs::symlink_metadata(&path).map_err(|_| "io")?;
    if !meta.file_type().is_file() || meta.len() > MAX_SOURCE_BYTES {
        return Err("too_large");
    }
    if Some(meta.mtime()) != node.mtime_sec || meta.len() != node.apparent_bytes {
        // Changed since the scan: parse on the next scan, for the version actually recorded.
        return Err("io");
    }
    let mut buf = Vec::with_capacity(meta.len() as usize);
    File::open(&path)
        .and_then(|f| f.take(MAX_SOURCE_BYTES).read_to_end(&mut buf))
        .map_err(|_| "io")?;
    if buf[..buf.len().min(BINARY_SNIFF_BYTES)].contains(&0) {
        return Err("binary");
    }
    let text = String::from_utf8_lossy(&buf);
    Ok(match syntax {
        SourceSyntax::Markdown => extract_markdown(&node.path_raw, &text, home),
        SourceSyntax::PlainText => extract_plain_text(&node.path_raw, &text, home),
    })
}

/// Refreshes the reference cache for all present source files. Returns the entries whose cache
/// must be (re)written and the full current map `source -> references`.
pub fn refresh_references(
    nodes: &[GraphNode],
    cache: &HashMap<EntityId, ReferenceCacheEntry>,
    home: Option<&[u8]>,
) -> (
    Vec<ReferenceCacheEntry>,
    HashMap<EntityId, Vec<ExtractedReference>>,
    ReferenceReport,
) {
    let mut report = ReferenceReport::default();
    let mut updated = Vec::new();
    let mut current = HashMap::new();
    for node in nodes {
        if node.state != EntityState::Present {
            continue;
        }
        let Some(syntax) = source_syntax(node) else {
            continue;
        };
        report.source_files += 1;
        if let Some(c) = cache.get(&node.entity_id) {
            if c.matches_version(node) {
                report.reused_from_cache += 1;
                report.references_found += c.references.len();
                current.insert(node.entity_id, c.references.clone());
                continue;
            }
        }
        match parse_file(node, syntax, home) {
            Ok(refs) => {
                report.parsed += 1;
                report.references_found += refs.len();
                current.insert(node.entity_id, refs.clone());
                updated.push(ReferenceCacheEntry {
                    source_entity_id: node.entity_id,
                    size_bytes: node.apparent_bytes,
                    mtime_sec: node.mtime_sec,
                    mtime_nsec: node.mtime_nsec,
                    references: refs,
                });
            }
            Err("too_large") => report.skipped_too_large += 1,
            Err("binary") => {
                report.skipped_binary += 1;
                // Cache the "no references" verdict so the file is not re-read every time.
                updated.push(ReferenceCacheEntry {
                    source_entity_id: node.entity_id,
                    size_bytes: node.apparent_bytes,
                    mtime_sec: node.mtime_sec,
                    mtime_nsec: node.mtime_nsec,
                    references: Vec::new(),
                });
            }
            Err(_) => report.read_errors += 1,
        }
    }
    (updated, current, report)
}
