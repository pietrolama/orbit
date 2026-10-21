#![allow(dead_code)]
//! Shared helpers for synthetic tests. Everything happens inside temporary directories.

use orbit_core::index::{index_scan, scan_config_for, IndexOptions, IndexReport};
use orbit_core::{
    scan_directory, CancelToken, EntityState, EntryType, GraphNode, OrbitStore, ScanResult,
};
use std::fs;
use std::io::Write;
use std::path::Path;
use tempfile::TempDir;

pub struct Env {
    pub dir: TempDir,
    pub db: TempDir,
    pub store: OrbitStore,
}

impl Env {
    pub fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let db = TempDir::new().unwrap();
        let store = OrbitStore::open(db.path().join("orbit.db")).unwrap();
        Self { dir, db, store }
    }

    pub fn root(&self) -> &Path {
        self.dir.path()
    }

    pub fn scan(&self) -> ScanResult {
        let cfg = scan_config_for(self.root()).unwrap();
        scan_directory(&cfg, &CancelToken::new())
    }

    pub fn index(&self) -> IndexReport {
        self.index_with(&IndexOptions::default())
    }

    pub fn index_with(&self, opts: &IndexOptions) -> IndexReport {
        let scan = self.scan();
        index_scan(&self.store, &scan, None, opts).unwrap()
    }

    /// Entity id currently associated with a path relative to the root.
    pub fn entity(&self, rel: &str) -> i64 {
        self.node(rel)
            .map(|n| n.entity_id)
            .unwrap_or_else(|| panic!("no entity for {}", rel))
    }

    pub fn node(&self, rel: &str) -> Option<GraphNode> {
        let abs = fs::canonicalize(self.root()).unwrap().join(rel);
        let abs = abs.to_string_lossy().into_owned();
        self.store
            .load_nodes(true)
            .unwrap()
            .into_iter()
            .filter(|n| n.path_display == abs)
            .max_by_key(|n| (n.state == EntityState::Present, n.entity_id))
    }
}

pub fn write(path: &Path, content: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    let mut f = fs::File::create(path).unwrap();
    f.write_all(content).unwrap();
}

/// Deterministic pseudo-random bytes.
pub fn bytes(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x & 0xff) as u8
        })
        .collect()
}

/// Sets the mtime of a file (seconds since epoch).
pub fn set_mtime(path: &Path, secs: i64) {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64);
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

/// Synthetic graph node (no filesystem).
pub fn node(
    id: i64,
    parent: Option<i64>,
    kind: EntryType,
    path: &str,
    size: u64,
    mtime: Option<i64>,
) -> GraphNode {
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    GraphNode {
        entity_id: id,
        kind,
        state: EntityState::Present,
        path_raw: path.as_bytes().to_vec(),
        path_display: path.to_string(),
        name,
        parent_entity_id: parent,
        depth: path.matches('/').count() as u32,
        apparent_bytes: size,
        allocated_bytes: size.div_ceil(4096) * 4096,
        subtree_apparent_bytes: size,
        subtree_files_count: u64::from(kind == EntryType::File),
        mtime_sec: mtime,
        mtime_nsec: mtime.map(|_| 123),
        dev: Some(1),
        ino: Some(id as u64 + 1000),
        nlink: 1,
        is_hardlink_duplicate: false,
        access_status: orbit_core::AccessStatus::Ok,
    }
}
