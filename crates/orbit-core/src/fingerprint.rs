//! Two-stage content fingerprints.
//!
//! * **Stage 1 — sample fingerprint**: BLAKE3 over `size || first 64 KiB || last 64 KiB`. Cheap,
//!   used only as a *filter*: equal samples produce a `FingerprintMatch`, never a verified duplicate
//!   — unless the file is small enough (<= 128 KiB) for the sample to cover the whole content.
//! * **Stage 2 — full hash** (optional, explicit): BLAKE3 over the full content, computed only for
//!   files whose sample already matches another file. Equal full hashes produce a
//!   `VerifiedDuplicate`.
//!
//! A fingerprint is bound to the observed version of the file (size, mtime, dev, ino). Before
//! reading, the file is re-`lstat`ed: if it changed since the scan, it is skipped rather than
//! fingerprinting a different version than the one recorded.

use crate::model::{EntityId, EntryType, GraphNode};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// Bytes read from each end of the file for the sample fingerprint.
pub const SAMPLE_CHUNK_BYTES: u64 = 64 * 1024;

/// A cached fingerprint tied to one observed version of a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub entity_id: EntityId,
    pub size_bytes: u64,
    pub mtime_sec: Option<i64>,
    pub mtime_nsec: Option<u32>,
    pub dev: Option<u64>,
    pub ino: Option<u64>,
    pub sample_hash: String,
    /// `true` when the sample covered the entire file (size <= 2 * [`SAMPLE_CHUNK_BYTES`]).
    pub sample_covers_full_file: bool,
    pub full_hash: Option<String>,
    pub computed_at_unix_ms: i64,
}

impl Fingerprint {
    /// `true` if this fingerprint describes exactly the version recorded in `node`.
    pub fn matches_version(&self, node: &GraphNode) -> bool {
        self.size_bytes == node.apparent_bytes
            && self.mtime_sec == node.mtime_sec
            && self.mtime_nsec == node.mtime_nsec
            && self.dev == node.dev
            && self.ino == node.ino
    }

    /// Hash proving full-content identity, if known (full hash, or a sample covering the file).
    pub fn content_hash(&self) -> Option<&str> {
        if let Some(h) = &self.full_hash {
            Some(h)
        } else if self.sample_covers_full_file {
            Some(&self.sample_hash)
        } else {
            None
        }
    }
}

/// Limits for one fingerprinting pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FingerprintBudget {
    /// Maximum number of files sampled in one pass.
    pub max_files: usize,
    /// Maximum number of bytes read by full verification in one pass.
    pub max_full_bytes: u64,
    /// Whether stage 2 (full hash) runs at all.
    pub verify_full: bool,
}

impl Default for FingerprintBudget {
    fn default() -> Self {
        Self {
            max_files: 20_000,
            max_full_bytes: 2 * 1024 * 1024 * 1024,
            verify_full: false,
        }
    }
}

/// Statistics of a fingerprinting pass.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FingerprintReport {
    pub candidate_files: usize,
    pub reused_from_cache: usize,
    pub sampled: usize,
    pub fully_hashed: usize,
    pub skipped_changed_since_scan: usize,
    pub skipped_budget: usize,
    pub read_errors: usize,
    pub bytes_read: u64,
}

fn hex(hash: &blake3::Hash) -> String {
    hash.to_hex().to_string()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Computes the sample hash of `path` given its expected size. Returns `(hash, covers_full, bytes_read)`.
pub fn sample_hash(path: &Path, size: u64) -> io::Result<(String, bool, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&size.to_le_bytes());
    let covers_full = size <= 2 * SAMPLE_CHUNK_BYTES;
    let mut read_total = 0u64;
    if covers_full {
        let mut buf = Vec::with_capacity(size as usize);
        file.read_to_end(&mut buf)?;
        read_total += buf.len() as u64;
        hasher.update(&buf);
    } else {
        let mut buf = vec![0u8; SAMPLE_CHUNK_BYTES as usize];
        file.read_exact(&mut buf)?;
        hasher.update(&buf);
        file.seek(SeekFrom::Start(size - SAMPLE_CHUNK_BYTES))?;
        file.read_exact(&mut buf)?;
        hasher.update(&buf);
        read_total += 2 * SAMPLE_CHUNK_BYTES;
    }
    Ok((hex(&hasher.finalize()), covers_full, read_total))
}

/// Computes the BLAKE3 hash of the full content of `path`.
pub fn full_hash(path: &Path) -> io::Result<(String, u64)> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((hex(&hasher.finalize()), total))
}

/// Re-checks that the file on disk is still the version recorded in `node`.
fn still_same_version(path: &Path, node: &GraphNode) -> bool {
    match fs::symlink_metadata(path) {
        Ok(m) => {
            m.file_type().is_file()
                && m.len() == node.apparent_bytes
                && Some(m.mtime()) == node.mtime_sec
                && u32::try_from(m.mtime_nsec()).ok() == node.mtime_nsec
                && Some(m.ino()) == node.ino
                && Some(m.dev()) == node.dev
        }
        Err(_) => false,
    }
}

/// Files eligible for fingerprinting: present regular non-empty files whose exact size is shared
/// with at least one other eligible file (a different size can never be a duplicate). Hard-link
/// duplicates are excluded (same inode, not a copy). Sorted by entity id for determinism.
pub fn fingerprint_candidates(nodes: &[GraphNode]) -> Vec<&GraphNode> {
    let mut by_size: BTreeMap<u64, Vec<&GraphNode>> = BTreeMap::new();
    for n in nodes {
        if n.kind == EntryType::File
            && n.state == crate::model::EntityState::Present
            && n.apparent_bytes > 0
            && !n.is_hardlink_duplicate
        {
            by_size.entry(n.apparent_bytes).or_default().push(n);
        }
    }
    let mut out: Vec<&GraphNode> = by_size
        .into_values()
        .filter(|v| v.len() >= 2)
        .flatten()
        .collect();
    out.sort_by_key(|n| n.entity_id);
    out
}

/// Refreshes fingerprints for all candidates, reusing valid cached ones. Returns the complete set
/// of fingerprints valid for the current versions plus a report. Pure I/O + hashing, no DB access.
pub fn refresh_fingerprints(
    nodes: &[GraphNode],
    cache: &HashMap<EntityId, Fingerprint>,
    budget: &FingerprintBudget,
) -> (Vec<Fingerprint>, FingerprintReport) {
    let candidates = fingerprint_candidates(nodes);
    let mut report = FingerprintReport {
        candidate_files: candidates.len(),
        ..Default::default()
    };
    let mut out: Vec<Fingerprint> = Vec::with_capacity(candidates.len());

    for node in candidates {
        if let Some(fp) = cache.get(&node.entity_id) {
            if fp.matches_version(node) {
                report.reused_from_cache += 1;
                out.push(fp.clone());
                continue;
            }
        }
        if report.sampled >= budget.max_files {
            report.skipped_budget += 1;
            continue;
        }
        let path = crate::scanner::raw_bytes_to_path_buf(&node.path_raw);
        if !still_same_version(&path, node) {
            report.skipped_changed_since_scan += 1;
            continue;
        }
        match sample_hash(&path, node.apparent_bytes) {
            Ok((hash, covers, read)) => {
                report.sampled += 1;
                report.bytes_read += read;
                out.push(Fingerprint {
                    entity_id: node.entity_id,
                    size_bytes: node.apparent_bytes,
                    mtime_sec: node.mtime_sec,
                    mtime_nsec: node.mtime_nsec,
                    dev: node.dev,
                    ino: node.ino,
                    sample_hash: hash,
                    sample_covers_full_file: covers,
                    full_hash: None,
                    computed_at_unix_ms: now_ms(),
                });
            }
            Err(_) => report.read_errors += 1,
        }
    }

    if budget.verify_full {
        verify_full_hashes(nodes, &mut out, budget, &mut report);
    }
    (out, report)
}

/// Stage 2: computes full hashes for files whose sample matches at least one other file and whose
/// sample does not already cover the full content.
fn verify_full_hashes(
    nodes: &[GraphNode],
    fps: &mut [Fingerprint],
    budget: &FingerprintBudget,
    report: &mut FingerprintReport,
) {
    let node_by_id: HashMap<EntityId, &GraphNode> =
        nodes.iter().map(|n| (n.entity_id, n)).collect();
    let mut groups: BTreeMap<(u64, String), Vec<usize>> = BTreeMap::new();
    for (i, fp) in fps.iter().enumerate() {
        if !fp.sample_covers_full_file {
            groups
                .entry((fp.size_bytes, fp.sample_hash.clone()))
                .or_default()
                .push(i);
        }
    }
    let mut full_bytes = 0u64;
    for idxs in groups.values().filter(|v| v.len() >= 2) {
        for &i in idxs {
            if fps[i].full_hash.is_some() {
                continue;
            }
            if full_bytes.saturating_add(fps[i].size_bytes) > budget.max_full_bytes {
                report.skipped_budget += 1;
                continue;
            }
            let Some(node) = node_by_id.get(&fps[i].entity_id) else {
                continue;
            };
            let path = crate::scanner::raw_bytes_to_path_buf(&node.path_raw);
            if !still_same_version(&path, node) {
                report.skipped_changed_since_scan += 1;
                continue;
            }
            match full_hash(&path) {
                Ok((h, read)) if read == fps[i].size_bytes => {
                    full_bytes += read;
                    report.bytes_read += read;
                    report.fully_hashed += 1;
                    fps[i].full_hash = Some(h);
                }
                // Size changed while reading: do not record a hash for an unknown version.
                Ok(_) => report.skipped_changed_since_scan += 1,
                Err(_) => report.read_errors += 1,
            }
        }
    }
}
