//! SQLite persistence for ORBIT (`docs/SCHEMA_V1.sql`).
//!
//! [`OrbitStore`] only holds the database path; every operation opens its own connection, so the
//! store is `Send + Sync + Clone` and can be shared between server threads. Multi-statement
//! updates run inside `BEGIN IMMEDIATE` transactions; WAL journaling gives readers a consistent view
//! while a scan is being recorded.

use crate::fingerprint::Fingerprint;
use crate::identity::{
    absolute_path_raw, parent_raw, resolve_identities, AbsenceVerdict, IdentityResolution,
    KnownEntity,
};
use crate::layout::{NodePosition, PositionSource};
use crate::model::{
    AccessStatus, Edge, EdgeOrigin, EdgeState, EntityId, EntityState, EntryType, GraphNode,
    IdentityMatch, RelationKind, ScanResult, ScanStatus,
};
use crate::references::{ExtractedReference, ReferenceCacheEntry};
pub use crate::sqlite::DbError;
use crate::sqlite::{Conn, Value};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const SCHEMA_V1_SQL: &str = include_str!("../../../docs/SCHEMA_V1.sql");
/// Highest schema version this binary understands.
pub const SUPPORTED_SCHEMA_VERSION: i64 = 1;
/// Name of the default canvas.
pub const MAIN_CANVAS: &str = "main";

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Header of a stored snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotHeader {
    pub id: i64,
    pub root_path_display: String,
    #[serde(skip)]
    pub root_path_raw: Vec<u8>,
    pub label: Option<String>,
    pub created_at_unix_ms: i64,
    pub scan_duration_ms: u64,
    pub status: String,
    pub total_apparent_bytes: u64,
    pub total_allocated_bytes: u64,
    pub total_files_count: u64,
    pub total_dirs_count: u64,
    pub permission_denied_count: u64,
    pub io_errors_count: u64,
    pub excluded_count: u64,
    pub new_entities_count: u64,
    pub matched_entities_count: u64,
    pub moved_entities_count: u64,
    pub ambiguous_entries_count: u64,
    pub unobserved_entities_count: u64,
    pub probably_deleted_entities_count: u64,
}

/// Summary returned by [`OrbitStore::record_scan`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordSummary {
    pub snapshot: SnapshotHeader,
    pub entries_recorded: usize,
}

/// One historical observation of an entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub snapshot_id: i64,
    pub snapshot_created_at_unix_ms: i64,
    pub path_display: String,
    pub apparent_bytes: u64,
    pub mtime_sec: Option<i64>,
    pub access_status: AccessStatus,
    pub identity_match: IdentityMatch,
    pub identity_confidence: f64,
}

/// A plausible identity link recorded for an entity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredIdentityCandidate {
    pub snapshot_id: i64,
    pub candidate_entity_id: EntityId,
    pub candidate_path_display: String,
    pub confidence: f64,
    pub reason: String,
}

/// Result of applying recomputed automatic edges.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EdgeApplyReport {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub marked_stale: usize,
    pub kept_hidden: usize,
}

/// A constellation with its members.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Constellation {
    pub id: i64,
    pub name: String,
    pub color: Option<String>,
    pub note: Option<String>,
    pub members: Vec<EntityId>,
    pub created_at_unix_ms: i64,
    pub updated_at_unix_ms: i64,
    pub deleted: bool,
    /// Position of the constellation's own spatial entity on the main canvas.
    pub x: Option<f64>,
    pub y: Option<f64>,
}

/// Counts for `orbit status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StoreStatus {
    pub db_path: String,
    pub schema_version: i64,
    pub snapshots: u64,
    pub entities_present: u64,
    pub entities_unobserved: u64,
    pub entities_probably_deleted: u64,
    pub edges_by_kind: BTreeMap<String, u64>,
    pub manual_edges: u64,
    pub hidden_edges: u64,
    pub stale_edges: u64,
    pub fingerprints: u64,
    pub verified_hashes: u64,
    pub positions: u64,
    pub pinned_positions: u64,
    pub constellations: u64,
}

/// Local SQLite store.
#[derive(Debug, Clone)]
pub struct OrbitStore {
    db_path: PathBuf,
}

impl OrbitStore {
    /// Opens (or creates) the database at `db_path`, applies migrations, and marks snapshots left
    /// in `saving` state by a crash as `interrupted`.
    pub fn open<P: AsRef<Path>>(db_path: P) -> Result<Self, DbError> {
        let store = Self {
            db_path: db_path.as_ref().to_path_buf(),
        };
        let conn = store.connect()?;
        conn.exec_batch("PRAGMA journal_mode = WAL;")?;
        let version = conn.query_i64("PRAGMA user_version", &[])?.unwrap_or(0);
        if version > SUPPORTED_SCHEMA_VERSION {
            return Err(DbError::SchemaTooNew {
                found: version,
                supported: SUPPORTED_SCHEMA_VERSION,
            });
        }
        if version < 1 {
            conn.transaction(|c| {
                c.exec_batch(SCHEMA_V1_SQL)?;
                c.execute(
                    "INSERT OR IGNORE INTO schema_migrations (version, applied_at_unix_ms) VALUES (1, ?)",
                    &[now_ms().into()],
                )?;
                c.exec_batch("PRAGMA user_version = 1;")?;
                Ok(())
            })?;
        }
        conn.execute(
            "UPDATE snapshots SET status = 'interrupted' WHERE status = 'saving'",
            &[],
        )?;
        conn.execute(
            "INSERT OR IGNORE INTO canvases (name, layout_seed, created_at_unix_ms) VALUES (?, 1, ?)",
            &[MAIN_CANVAS.into(), now_ms().into()],
        )?;
        Ok(store)
    }

    /// Default location: `$ORBIT_DB`, else `$XDG_DATA_HOME/orbit/orbit.db`, else
    /// `~/.local/share/orbit/orbit.db`.
    pub fn default_path() -> PathBuf {
        if let Some(p) = env::var_os("ORBIT_DB") {
            return PathBuf::from(p);
        }
        let base = if let Some(xdg) = env::var_os("XDG_DATA_HOME") {
            PathBuf::from(xdg)
        } else if let Some(home) = env::var_os("HOME") {
            PathBuf::from(home).join(".local/share")
        } else {
            env::temp_dir()
        };
        base.join("orbit").join("orbit.db")
    }

    pub fn open_default() -> Result<Self, DbError> {
        Self::open(Self::default_path())
    }

    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    pub(crate) fn connect(&self) -> Result<Conn, DbError> {
        let conn = Conn::open(&self.db_path)?;
        conn.exec_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")?;
        Ok(conn)
    }

    /// Executes raw SQL. Only for tests that simulate crashes or foreign schema versions.
    #[doc(hidden)]
    pub fn exec_sql_for_tests(&self, sql: &str) -> Result<(), DbError> {
        self.connect()?.exec_batch(sql)
    }

    pub fn schema_version(&self) -> Result<i64, DbError> {
        Ok(self
            .connect()?
            .query_i64("PRAGMA user_version", &[])?
            .unwrap_or(0))
    }

    // ============================================================================================
    // Snapshots and identity
    // ============================================================================================

    /// Loads every known entity (all states) for identity resolution.
    pub fn load_known_entities(&self) -> Result<Vec<KnownEntity>, DbError> {
        let conn = self.connect()?;
        let mut st = conn.prepare(
            "SELECT id, kind, state, last_path_raw, last_dev, last_ino, last_apparent_bytes,
                    last_mtime_sec, last_mtime_nsec, last_seen_snapshot_id, last_name_display,
                    parent_entity_id, depth, last_allocated_bytes, subtree_apparent_bytes,
                    subtree_files_count, nlink, is_hardlink_duplicate, access_status
             FROM file_entities ORDER BY id",
        )?;
        let mut out = Vec::new();
        while st.step()? {
            let kind = EntryType::from_db(&st.col_text(1));
            let path_raw = st.col_blob(3);
            let dev = st.col_opt_i64(4);
            let ino = st.col_opt_i64(5);
            let mtime_sec = st.col_opt_i64(7);
            let mtime_nsec = st.col_opt_i64(8);
            let sig = RowSig {
                kind: kind.as_db(),
                path: &path_raw,
                name: &st.col_text(10),
                parent: st.col_opt_i64(11),
                depth: st.col_i64(12),
                dev,
                ino,
                apparent: st.col_i64(6),
                allocated: st.col_i64(13),
                subtree_apparent: st.col_i64(14),
                subtree_files: st.col_i64(15),
                mtime_sec,
                mtime_nsec,
                nlink: st.col_i64(16),
                hardlink_dup: st.col_bool(17),
                access: &st.col_text(18),
            }
            .hash();
            out.push(KnownEntity {
                id: st.col_i64(0),
                kind,
                state: EntityState::from_db(&st.col_text(2)),
                path_raw,
                dev: dev.map(|v| v as u64),
                ino: ino.map(|v| v as u64),
                apparent_bytes: st.col_i64(6) as u64,
                mtime_sec,
                mtime_nsec: mtime_nsec.map(|v| v as u32),
                last_seen_snapshot_id: st.col_i64(9),
                row_signature: sig,
            });
        }
        Ok(out)
    }

    /// Records a scan as a new snapshot, resolving identities against known entities.
    ///
    /// Cancelled and failed scans are refused: their observations are too incomplete to be used as
    /// history. Partial scans are recorded; their gaps make missing entities `unobserved`, never
    /// `probably_deleted`.
    pub fn record_scan(
        &self,
        scan: &ScanResult,
        label: Option<&str>,
    ) -> Result<RecordSummary, DbError> {
        if matches!(scan.status, ScanStatus::Cancelled | ScanStatus::Failed) {
            return Err(DbError::Invalid(format!(
                "una scansione con stato '{}' non può essere registrata come snapshot",
                scan.status.as_db()
            )));
        }
        if !scan.root_path_raw.starts_with(b"/") {
            return Err(DbError::Invalid(
                "la radice della scansione deve essere un percorso assoluto".to_string(),
            ));
        }
        let known = self.load_known_entities()?;
        let resolution = resolve_identities(scan, &known);
        let conn = self.connect()?;
        let snapshot_id =
            conn.transaction(|c| write_snapshot(c, scan, label, &known, &resolution))?;
        let snapshot = self.snapshot_header(snapshot_id)?;
        Ok(RecordSummary {
            snapshot,
            entries_recorded: scan.entries.len(),
        })
    }

    pub fn snapshot_header(&self, id: i64) -> Result<SnapshotHeader, DbError> {
        let conn = self.connect()?;
        let mut st = conn.prepare(&format!("{} WHERE id = ?", SNAPSHOT_SELECT))?;
        st.bind_all(&[id.into()])?;
        if st.step()? {
            Ok(read_snapshot_header(&st))
        } else {
            Err(DbError::NotFound(format!("snapshot {}", id)))
        }
    }

    /// Lists snapshots, most recent first.
    pub fn list_snapshots(&self) -> Result<Vec<SnapshotHeader>, DbError> {
        let conn = self.connect()?;
        let mut st = conn.prepare(&format!(
            "{} ORDER BY created_at_unix_ms DESC, id DESC",
            SNAPSHOT_SELECT
        ))?;
        let mut out = Vec::new();
        while st.step()? {
            out.push(read_snapshot_header(&st));
        }
        Ok(out)
    }

    /// Deletes one snapshot's observations. Entities, positions and relations are kept.
    pub fn delete_snapshot(&self, id: i64) -> Result<bool, DbError> {
        let conn = self.connect()?;
        Ok(conn.execute("DELETE FROM snapshots WHERE id = ?", &[id.into()])? > 0)
    }

    /// Keeps the `keep` most recent usable snapshots per root and deletes older observations.
    /// Interrupted snapshots are always deleted. Returns the deleted snapshot ids.
    pub fn prune_snapshots(&self, keep: usize) -> Result<Vec<i64>, DbError> {
        let snaps = self.list_snapshots()?;
        let mut seen: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut to_delete = Vec::new();
        for s in snaps {
            if s.status == "interrupted" || s.status == "saving" {
                to_delete.push(s.id);
                continue;
            }
            let c = seen.entry(s.root_path_raw.clone()).or_default();
            *c += 1;
            if *c > keep.max(1) {
                to_delete.push(s.id);
            }
        }
        let conn = self.connect()?;
        conn.transaction(|c| {
            for id in &to_delete {
                c.execute("DELETE FROM snapshots WHERE id = ?", &[(*id).into()])?;
            }
            Ok(())
        })?;
        Ok(to_delete)
    }

    /// Observation history of one entity, oldest first.
    pub fn entity_observations(&self, entity: EntityId) -> Result<Vec<Observation>, DbError> {
        let conn = self.connect()?;
        let mut st = conn.prepare(
            "SELECT e.snapshot_id, s.created_at_unix_ms, e.path_display, e.apparent_bytes, e.mtime_sec,
                    e.access_status, e.identity_match, e.identity_confidence
             FROM snapshot_entries e JOIN snapshots s ON s.id = e.snapshot_id
             WHERE e.entity_id = ? ORDER BY s.created_at_unix_ms, e.snapshot_id",
        )?;
        st.bind_all(&[entity.into()])?;
        let mut out = Vec::new();
        while st.step()? {
            out.push(Observation {
                snapshot_id: st.col_i64(0),
                snapshot_created_at_unix_ms: st.col_i64(1),
                path_display: st.col_text(2),
                apparent_bytes: st.col_i64(3) as u64,
                mtime_sec: st.col_opt_i64(4),
                access_status: AccessStatus::from_db(&st.col_text(5)),
                identity_match: IdentityMatch::from_db(&st.col_text(6)),
                identity_confidence: st.col_f64(7),
            });
        }
        Ok(out)
    }

    /// Plausible identity links recorded for an entity.
    pub fn identity_candidates(
        &self,
        entity: EntityId,
    ) -> Result<Vec<StoredIdentityCandidate>, DbError> {
        let conn = self.connect()?;
        let mut st = conn.prepare(
            "SELECT c.snapshot_id, c.candidate_entity_id, f.last_path_display, c.confidence, c.reason
             FROM identity_candidates c JOIN file_entities f ON f.id = c.candidate_entity_id
             WHERE c.entity_id = ? ORDER BY c.snapshot_id DESC, c.confidence DESC",
        )?;
        st.bind_all(&[entity.into()])?;
        let mut out = Vec::new();
        while st.step()? {
            out.push(StoredIdentityCandidate {
                snapshot_id: st.col_i64(0),
                candidate_entity_id: st.col_i64(1),
                candidate_path_display: st.col_text(2),
                confidence: st.col_f64(3),
                reason: st.col_text(4),
            });
        }
        Ok(out)
    }

    /// Raw rows of one snapshot used by the history diff: `entity_id -> (path, size, mtime, status)`.
    pub(crate) fn snapshot_rows(
        &self,
        snapshot_id: i64,
    ) -> Result<HashMap<EntityId, SnapshotRow>, DbError> {
        let conn = self.connect()?;
        let mut st = conn.prepare(
            "SELECT entity_id, path_display, entry_type, apparent_bytes, mtime_sec, mtime_nsec,
                    access_status, identity_match
             FROM snapshot_entries WHERE snapshot_id = ? AND entity_id IS NOT NULL",
        )?;
        st.bind_all(&[snapshot_id.into()])?;
        let mut out = HashMap::new();
        while st.step()? {
            out.insert(
                st.col_i64(0),
                SnapshotRow {
                    path_display: st.col_text(1),
                    kind: EntryType::from_db(&st.col_text(2)),
                    apparent_bytes: st.col_i64(3) as u64,
                    mtime_sec: st.col_opt_i64(4),
                    mtime_nsec: st.col_opt_i64(5).map(|v| v as u32),
                    access_status: AccessStatus::from_db(&st.col_text(6)),
                    identity_match: IdentityMatch::from_db(&st.col_text(7)),
                },
            );
        }
        Ok(out)
    }

    // ============================================================================================
    // Graph nodes
    // ============================================================================================

    /// Loads entities as graph nodes. With `include_absent == false` only `present` entities.
    pub fn load_nodes(&self, include_absent: bool) -> Result<Vec<GraphNode>, DbError> {
        let conn = self.connect()?;
        let sql = format!(
            "SELECT id, kind, state, last_path_raw, last_path_display, last_name_display, parent_entity_id,
                    depth, last_apparent_bytes, last_allocated_bytes, subtree_apparent_bytes,
                    subtree_files_count, last_mtime_sec, last_mtime_nsec, last_dev, last_ino, nlink,
                    is_hardlink_duplicate, access_status
             FROM file_entities {} ORDER BY id",
            if include_absent { "" } else { "WHERE state = 'present'" }
        );
        let mut st = conn.prepare(&sql)?;
        let mut out = Vec::new();
        while st.step()? {
            out.push(GraphNode {
                entity_id: st.col_i64(0),
                kind: EntryType::from_db(&st.col_text(1)),
                state: EntityState::from_db(&st.col_text(2)),
                path_raw: st.col_blob(3),
                path_display: st.col_text(4),
                name: st.col_text(5),
                parent_entity_id: st.col_opt_i64(6),
                depth: st.col_i64(7) as u32,
                apparent_bytes: st.col_i64(8) as u64,
                allocated_bytes: st.col_i64(9) as u64,
                subtree_apparent_bytes: st.col_i64(10) as u64,
                subtree_files_count: st.col_i64(11) as u64,
                mtime_sec: st.col_opt_i64(12),
                mtime_nsec: st.col_opt_i64(13).map(|v| v as u32),
                dev: st.col_opt_i64(14).map(|v| v as u64),
                ino: st.col_opt_i64(15).map(|v| v as u64),
                nlink: st.col_i64(16) as u64,
                is_hardlink_duplicate: st.col_bool(17),
                access_status: AccessStatus::from_db(&st.col_text(18)),
            });
        }
        Ok(out)
    }

    pub fn entity_exists(&self, id: EntityId) -> Result<bool, DbError> {
        let conn = self.connect()?;
        Ok(conn
            .query_i64("SELECT 1 FROM file_entities WHERE id = ?", &[id.into()])?
            .is_some())
    }

    // ============================================================================================
    // Fingerprints and references
    // ============================================================================================

    pub fn load_fingerprints(&self) -> Result<HashMap<EntityId, Fingerprint>, DbError> {
        let conn = self.connect()?;
        let mut st = conn.prepare(
            "SELECT entity_id, size_bytes, mtime_sec, mtime_nsec, dev, ino, sample_hash,
                    sample_covers_full_file, full_hash, computed_at_unix_ms FROM fingerprints",
        )?;
        let mut out = HashMap::new();
        while st.step()? {
            let fp = Fingerprint {
                entity_id: st.col_i64(0),
                size_bytes: st.col_i64(1) as u64,
                mtime_sec: st.col_opt_i64(2),
                mtime_nsec: st.col_opt_i64(3).map(|v| v as u32),
                dev: st.col_opt_i64(4).map(|v| v as u64),
                ino: st.col_opt_i64(5).map(|v| v as u64),
                sample_hash: st.col_text(6),
                sample_covers_full_file: st.col_bool(7),
                full_hash: st.col_opt_text(8),
                computed_at_unix_ms: st.col_i64(9),
            };
            out.insert(fp.entity_id, fp);
        }
        Ok(out)
    }

    /// Upserts fingerprints (one row per entity: a new version replaces the old one).
    pub fn save_fingerprints(&self, fps: &[Fingerprint]) -> Result<(), DbError> {
        let conn = self.connect()?;
        conn.transaction(|c| {
            let mut st = c.prepare(
                "INSERT INTO fingerprints (entity_id, size_bytes, mtime_sec, mtime_nsec, dev, ino,
                    sample_hash, sample_covers_full_file, full_hash, computed_at_unix_ms)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(entity_id) DO UPDATE SET size_bytes = excluded.size_bytes,
                    mtime_sec = excluded.mtime_sec, mtime_nsec = excluded.mtime_nsec,
                    dev = excluded.dev, ino = excluded.ino, sample_hash = excluded.sample_hash,
                    sample_covers_full_file = excluded.sample_covers_full_file,
                    full_hash = excluded.full_hash, computed_at_unix_ms = excluded.computed_at_unix_ms",
            )?;
            for f in fps {
                st.run(&[
                    f.entity_id.into(),
                    (f.size_bytes as i64).into(),
                    f.mtime_sec.into(),
                    f.mtime_nsec.map(i64::from).into(),
                    f.dev.map(|v| v as i64).into(),
                    f.ino.map(|v| v as i64).into(),
                    f.sample_hash.as_str().into(),
                    f.sample_covers_full_file.into(),
                    f.full_hash.as_deref().into(),
                    f.computed_at_unix_ms.into(),
                ])?;
            }
            Ok(())
        })
    }

    pub fn load_reference_cache(&self) -> Result<HashMap<EntityId, ReferenceCacheEntry>, DbError> {
        let conn = self.connect()?;
        let mut st = conn.prepare(
            "SELECT source_entity_id, source_size_bytes, source_mtime_sec, source_mtime_nsec,
                    target_path_raw, line, syntax FROM extracted_references ORDER BY id",
        )?;
        let mut out: HashMap<EntityId, ReferenceCacheEntry> = HashMap::new();
        while st.step()? {
            let id = st.col_i64(0);
            let entry = out.entry(id).or_insert_with(|| ReferenceCacheEntry {
                source_entity_id: id,
                size_bytes: st.col_i64(1) as u64,
                mtime_sec: st.col_opt_i64(2),
                mtime_nsec: st.col_opt_i64(3).map(|v| v as u32),
                references: Vec::new(),
            });
            let raw = st.col_blob(4);
            if !raw.is_empty() {
                entry.references.push(ExtractedReference {
                    target_display: String::from_utf8_lossy(&raw).into_owned(),
                    target_path_raw: raw,
                    line: st.col_i64(5) as u32,
                    syntax: st.col_text(6),
                });
            }
        }
        Ok(out)
    }

    /// Replaces the cached references of the given source versions.
    pub fn save_reference_cache(&self, entries: &[ReferenceCacheEntry]) -> Result<(), DbError> {
        let conn = self.connect()?;
        conn.transaction(|c| {
            let mut del = c.prepare("DELETE FROM extracted_references WHERE source_entity_id = ?")?;
            let mut ins = c.prepare(
                "INSERT INTO extracted_references (source_entity_id, source_size_bytes, source_mtime_sec,
                    source_mtime_nsec, target_path_raw, line, syntax) VALUES (?, ?, ?, ?, ?, ?, ?)",
            )?;
            for e in entries {
                del.run(&[e.source_entity_id.into()])?;
                let base = [
                    Value::I64(e.source_entity_id),
                    Value::I64(e.size_bytes as i64),
                    e.mtime_sec.into(),
                    e.mtime_nsec.map(i64::from).into(),
                ];
                if e.references.is_empty() {
                    ins.run(&[base[0], base[1], base[2], base[3], Value::Null, 0i64.into(), "none".into()])?;
                }
                for r in &e.references {
                    ins.run(&[
                        base[0],
                        base[1],
                        base[2],
                        base[3],
                        r.target_path_raw.as_slice().into(),
                        i64::from(r.line).into(),
                        r.syntax.as_str().into(),
                    ])?;
                }
            }
            Ok(())
        })
    }

    pub fn load_co_activity(&self) -> Result<Vec<(EntityId, EntityId, u32)>, DbError> {
        let conn = self.connect()?;
        let mut st = conn.prepare(
            "SELECT entity_a, entity_b, co_changes FROM co_activity ORDER BY entity_a, entity_b",
        )?;
        let mut out = Vec::new();
        while st.step()? {
            out.push((st.col_i64(0), st.col_i64(1), st.col_i64(2) as u32));
        }
        Ok(out)
    }

    // ============================================================================================
    // Edges
    // ============================================================================================

    /// Loads stored edges. `states == None` loads all states.
    pub fn load_edges(&self, states: Option<&[EdgeState]>) -> Result<Vec<Edge>, DbError> {
        let conn = self.connect()?;
        let filter = match states {
            Some(list) if !list.is_empty() => format!(
                "WHERE state IN ({})",
                list.iter()
                    .map(|s| format!("'{}'", s.as_db()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            _ => String::new(),
        };
        let mut st = conn.prepare(&format!(
            "SELECT id, src_entity_id, dst_entity_id, kind, origin, weight, confidence, provenance,
                    reasons_json, state FROM edges {} ORDER BY id",
            filter
        ))?;
        let mut out = Vec::new();
        while st.step()? {
            out.push(read_edge(&st));
        }
        Ok(out)
    }

    pub fn edge(&self, id: i64) -> Result<Edge, DbError> {
        let conn = self.connect()?;
        let mut st = conn.prepare(
            "SELECT id, src_entity_id, dst_entity_id, kind, origin, weight, confidence, provenance,
                    reasons_json, state FROM edges WHERE id = ?",
        )?;
        st.bind_all(&[id.into()])?;
        if st.step()? {
            Ok(read_edge(&st))
        } else {
            Err(DbError::NotFound(format!("relazione {}", id)))
        }
    }

    /// Applies a freshly computed set of automatic edges as a diff against the stored ones.
    ///
    /// * new keys are inserted as `active`;
    /// * existing keys are refreshed (weight, confidence, reasons); `hidden` stays `hidden`,
    ///   `stale` becomes `active` again;
    /// * stored active automatic edges that were not produced are marked `stale`.
    ///
    /// Manual edges are never touched.
    pub fn apply_auto_edges(&self, computed: &[Edge]) -> Result<EdgeApplyReport, DbError> {
        let existing: HashMap<(EntityId, EntityId, RelationKind), Edge> = self
            .load_edges(None)?
            .into_iter()
            .filter(|e| e.origin == EdgeOrigin::Auto)
            .map(|e| ((e.src, e.dst, e.kind), e))
            .collect();
        let mut report = EdgeApplyReport::default();
        let now = now_ms();
        let conn = self.connect()?;
        conn.transaction(|c| {
            let mut ins = c.prepare(
                "INSERT INTO edges (src_entity_id, dst_entity_id, kind, origin, weight, confidence,
                    provenance, reasons_json, state, created_at_unix_ms, updated_at_unix_ms)
                 VALUES (?, ?, ?, 'auto', ?, ?, ?, ?, 'active', ?, ?)",
            )?;
            let mut upd = c.prepare(
                "UPDATE edges SET weight = ?, confidence = ?, provenance = ?, reasons_json = ?,
                    state = ?, updated_at_unix_ms = ? WHERE id = ?",
            )?;
            let mut stale =
                c.prepare("UPDATE edges SET state = 'stale', updated_at_unix_ms = ? WHERE id = ?")?;
            let mut produced: HashSet<(EntityId, EntityId, RelationKind)> = HashSet::new();
            for e in computed {
                if e.src == e.dst || e.origin != EdgeOrigin::Auto {
                    continue;
                }
                let key = (e.src, e.dst, e.kind);
                produced.insert(key);
                let reasons = serde_json::to_string(&e.reasons).unwrap_or_else(|_| "[]".into());
                match existing.get(&key) {
                    None => {
                        ins.run(&[
                            e.src.into(),
                            e.dst.into(),
                            e.kind.as_db().into(),
                            e.weight.into(),
                            e.confidence.into(),
                            e.provenance.as_str().into(),
                            reasons.as_str().into(),
                            now.into(),
                            now.into(),
                        ])?;
                        report.inserted += 1;
                    }
                    Some(old) => {
                        let new_state = if old.state == EdgeState::Hidden {
                            report.kept_hidden += 1;
                            EdgeState::Hidden
                        } else {
                            EdgeState::Active
                        };
                        let same = (old.weight - e.weight).abs() < 1e-9
                            && (old.confidence - e.confidence).abs() < 1e-9
                            && old.reasons == e.reasons
                            && old.provenance == e.provenance
                            && old.state == new_state;
                        if same {
                            report.unchanged += 1;
                        } else {
                            upd.run(&[
                                e.weight.into(),
                                e.confidence.into(),
                                e.provenance.as_str().into(),
                                reasons.as_str().into(),
                                new_state.as_db().into(),
                                now.into(),
                                old.id.unwrap_or(0).into(),
                            ])?;
                            report.updated += 1;
                        }
                    }
                }
            }
            let mut stale_ids: Vec<i64> = existing
                .iter()
                .filter(|(k, e)| !produced.contains(k) && e.state == EdgeState::Active)
                .filter_map(|(_, e)| e.id)
                .collect();
            stale_ids.sort_unstable();
            for id in stale_ids {
                stale.run(&[now.into(), id.into()])?;
                report.marked_stale += 1;
            }
            Ok(())
        })?;
        Ok(report)
    }

    /// Creates (or restores) a manual association between two entities.
    pub fn create_manual_edge(
        &self,
        a: EntityId,
        b: EntityId,
        note: Option<&str>,
    ) -> Result<Edge, DbError> {
        if a == b {
            return Err(DbError::Invalid(
                "una relazione deve collegare due entità diverse".to_string(),
            ));
        }
        for id in [a, b] {
            if !self.entity_exists(id)? {
                return Err(DbError::NotFound(format!("entità {}", id)));
            }
        }
        let (src, dst) = if a < b { (a, b) } else { (b, a) };
        let mut reasons = vec!["associazione creata dall'utente".to_string()];
        if let Some(n) = note.map(str::trim).filter(|n| !n.is_empty()) {
            reasons.push(format!("nota: {}", n.chars().take(500).collect::<String>()));
        }
        let reasons_json = serde_json::to_string(&reasons).unwrap_or_else(|_| "[]".into());
        let now = now_ms();
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO edges (src_entity_id, dst_entity_id, kind, origin, weight, confidence, provenance,
                reasons_json, state, created_at_unix_ms, updated_at_unix_ms)
             VALUES (?, ?, 'manual_association', 'manual', 1.0, 1.0, 'user', ?, 'active', ?, ?)
             ON CONFLICT(src_entity_id, dst_entity_id, kind, origin) DO UPDATE SET
                state = 'active', reasons_json = excluded.reasons_json, updated_at_unix_ms = excluded.updated_at_unix_ms",
            &[src.into(), dst.into(), reasons_json.as_str().into(), now.into(), now.into()],
        )?;
        let id = conn
            .query_i64(
                "SELECT id FROM edges WHERE src_entity_id = ? AND dst_entity_id = ? AND kind = 'manual_association' AND origin = 'manual'",
                &[src.into(), dst.into()],
            )?
            .unwrap_or(0);
        drop(conn);
        self.edge(id)
    }

    /// Changes the state of an edge following the ownership rules:
    /// automatic edges can be hidden or shown (`active`), manual edges can be deleted (soft) or
    /// restored. Returns the updated edge.
    pub fn set_edge_state(&self, id: i64, state: EdgeState) -> Result<Edge, DbError> {
        let edge = self.edge(id)?;
        let allowed = matches!(
            (edge.origin, state),
            (EdgeOrigin::Auto, EdgeState::Hidden | EdgeState::Active)
                | (EdgeOrigin::Manual, EdgeState::Deleted | EdgeState::Active)
        );
        if !allowed {
            return Err(DbError::Invalid(format!(
                "transizione non permessa per una relazione {}: {}",
                edge.origin.as_db(),
                state.as_db()
            )));
        }
        if edge.origin == EdgeOrigin::Auto
            && edge.state == EdgeState::Stale
            && state == EdgeState::Active
        {
            return Err(DbError::Invalid(
                "una relazione automatica obsoleta torna attiva solo se il motore la ricalcola"
                    .to_string(),
            ));
        }
        let conn = self.connect()?;
        conn.execute(
            "UPDATE edges SET state = ?, updated_at_unix_ms = ? WHERE id = ?",
            &[state.as_db().into(), now_ms().into(), id.into()],
        )?;
        drop(conn);
        self.edge(id)
    }

    // ============================================================================================
    // Space: spatial entities and positions
    // ============================================================================================

    pub fn canvas_id(&self, name: &str) -> Result<i64, DbError> {
        let conn = self.connect()?;
        conn.query_i64("SELECT id FROM canvases WHERE name = ?", &[name.into()])?
            .ok_or_else(|| DbError::NotFound(format!("canvas {}", name)))
    }

    pub fn canvas_seed(&self, canvas_id: i64) -> Result<u64, DbError> {
        let conn = self.connect()?;
        Ok(conn
            .query_i64(
                "SELECT layout_seed FROM canvases WHERE id = ?",
                &[canvas_id.into()],
            )?
            .unwrap_or(1) as u64)
    }

    pub fn set_canvas_seed(&self, canvas_id: i64, seed: u64) -> Result<(), DbError> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE canvases SET layout_seed = ? WHERE id = ?",
            &[(seed as i64).into(), canvas_id.into()],
        )?;
        Ok(())
    }

    /// Loads positions of file entities on a canvas.
    pub fn load_positions(
        &self,
        canvas_id: i64,
    ) -> Result<HashMap<EntityId, NodePosition>, DbError> {
        let conn = self.connect()?;
        let mut st = conn.prepare(
            "SELECT s.file_entity_id, p.x, p.y, p.radius, p.pinned, p.source, p.cluster_id, p.layout_version
             FROM node_positions p JOIN spatial_entities s ON s.id = p.spatial_entity_id
             WHERE p.canvas_id = ? AND s.entity_type = 'file_entity'",
        )?;
        st.bind_all(&[canvas_id.into()])?;
        let mut out = HashMap::new();
        while st.step()? {
            let id = st.col_i64(0);
            out.insert(
                id,
                NodePosition {
                    entity_id: id,
                    x: st.col_f64(1),
                    y: st.col_f64(2),
                    radius: st.col_f64(3),
                    pinned: st.col_bool(4),
                    source: PositionSource::from_db(&st.col_text(5)),
                    cluster_id: st.col_opt_i64(6),
                    layout_version: st.col_i64(7) as u32,
                },
            );
        }
        Ok(out)
    }

    /// Writes layout-engine positions. Locked rows (pinned or manual) are never overwritten,
    /// enforced in SQL as well as in the layout engine.
    pub fn save_auto_positions(
        &self,
        canvas_id: i64,
        positions: &[NodePosition],
    ) -> Result<usize, DbError> {
        self.write_positions(canvas_id, positions, false)
    }

    /// Writes user positions (drag). They become `source = manual`.
    pub fn save_manual_positions(
        &self,
        canvas_id: i64,
        positions: &[NodePosition],
    ) -> Result<usize, DbError> {
        for p in positions {
            if !p.x.is_finite() || !p.y.is_finite() || p.x.abs() > 1e9 || p.y.abs() > 1e9 {
                return Err(DbError::Invalid("coordinate non valide".to_string()));
            }
        }
        self.write_positions(canvas_id, positions, true)
    }

    fn write_positions(
        &self,
        canvas_id: i64,
        positions: &[NodePosition],
        manual: bool,
    ) -> Result<usize, DbError> {
        let now = now_ms();
        let conn = self.connect()?;
        conn.transaction(|c| {
            let mut ensure = c.prepare(
                "INSERT OR IGNORE INTO spatial_entities (entity_type, file_entity_id, created_at_unix_ms)
                 VALUES ('file_entity', ?, ?)",
            )?;
            let mut sid = c.prepare("SELECT id FROM spatial_entities WHERE file_entity_id = ?")?;
            let sql = if manual {
                "INSERT INTO node_positions (spatial_entity_id, canvas_id, x, y, radius, pinned, source,
                    cluster_id, layout_version, updated_at_unix_ms)
                 VALUES (?, ?, ?, ?, ?, ?, 'manual', ?, ?, ?)
                 ON CONFLICT(spatial_entity_id, canvas_id) DO UPDATE SET x = excluded.x, y = excluded.y,
                    source = 'manual', pinned = MAX(node_positions.pinned, excluded.pinned),
                    updated_at_unix_ms = excluded.updated_at_unix_ms"
            } else {
                "INSERT INTO node_positions (spatial_entity_id, canvas_id, x, y, radius, pinned, source,
                    cluster_id, layout_version, updated_at_unix_ms)
                 VALUES (?, ?, ?, ?, ?, ?, 'auto', ?, ?, ?)
                 ON CONFLICT(spatial_entity_id, canvas_id) DO UPDATE SET x = excluded.x, y = excluded.y,
                    radius = excluded.radius, cluster_id = excluded.cluster_id,
                    layout_version = excluded.layout_version, updated_at_unix_ms = excluded.updated_at_unix_ms
                 WHERE node_positions.pinned = 0 AND node_positions.source = 'auto'"
            };
            let mut up = c.prepare(sql)?;
            let mut written = 0;
            for p in positions {
                ensure.run(&[p.entity_id.into(), now.into()])?;
                sid.reset();
                sid.bind_all(&[p.entity_id.into()])?;
                if !sid.step()? {
                    continue;
                }
                let spatial = sid.col_i64(0);
                up.run(&[
                    spatial.into(),
                    canvas_id.into(),
                    p.x.into(),
                    p.y.into(),
                    p.radius.into(),
                    p.pinned.into(),
                    p.cluster_id.into(),
                    i64::from(p.layout_version).into(),
                    now.into(),
                ])?;
                written += c.changes();
            }
            Ok(written)
        })
    }

    /// Pins or unpins entities on a canvas. Pinned nodes are never moved automatically.
    pub fn set_pinned(
        &self,
        canvas_id: i64,
        ids: &[EntityId],
        pinned: bool,
    ) -> Result<usize, DbError> {
        let now = now_ms();
        let conn = self.connect()?;
        conn.transaction(|c| {
            let mut st = c.prepare(
                "UPDATE node_positions SET pinned = ?, updated_at_unix_ms = ?
                 WHERE canvas_id = ? AND spatial_entity_id = (SELECT id FROM spatial_entities WHERE file_entity_id = ?)",
            )?;
            let mut n = 0;
            for id in ids {
                st.run(&[pinned.into(), now.into(), canvas_id.into(), (*id).into()])?;
                n += c.changes();
            }
            Ok(n)
        })
    }

    /// Returns a locked node to automatic control (unpinned, `source = auto`); its coordinates are
    /// kept until an explicit re-layout.
    pub fn release_positions(&self, canvas_id: i64, ids: &[EntityId]) -> Result<usize, DbError> {
        let now = now_ms();
        let conn = self.connect()?;
        conn.transaction(|c| {
            let mut st = c.prepare(
                "UPDATE node_positions SET pinned = 0, source = 'auto', updated_at_unix_ms = ?
                 WHERE canvas_id = ? AND spatial_entity_id = (SELECT id FROM spatial_entities WHERE file_entity_id = ?)",
            )?;
            let mut n = 0;
            for id in ids {
                st.run(&[now.into(), canvas_id.into(), (*id).into()])?;
                n += c.changes();
            }
            Ok(n)
        })
    }

    // ============================================================================================
    // Constellations
    // ============================================================================================

    pub fn create_constellation(
        &self,
        name: &str,
        color: Option<&str>,
        members: &[EntityId],
        position: Option<(f64, f64)>,
    ) -> Result<Constellation, DbError> {
        let name = name.trim();
        if name.is_empty() || name.chars().count() > 120 {
            return Err(DbError::Invalid(
                "nome della costellazione non valido".to_string(),
            ));
        }
        if let Some(c) = color {
            if !is_valid_color(c) {
                return Err(DbError::Invalid(
                    "colore non valido (atteso #rrggbb)".to_string(),
                ));
            }
        }
        for id in members {
            if !self.entity_exists(*id)? {
                return Err(DbError::NotFound(format!("entità {}", id)));
            }
        }
        let now = now_ms();
        let canvas = self.canvas_id(MAIN_CANVAS)?;
        let conn = self.connect()?;
        let id = conn.transaction(|c| {
            c.execute(
                "INSERT INTO constellations (name, color, created_at_unix_ms, updated_at_unix_ms) VALUES (?, ?, ?, ?)",
                &[name.into(), color.into(), now.into(), now.into()],
            )?;
            let id = c.last_insert_rowid();
            c.execute(
                "INSERT INTO spatial_entities (entity_type, constellation_id, created_at_unix_ms) VALUES ('constellation', ?, ?)",
                &[id.into(), now.into()],
            )?;
            let spatial = c.last_insert_rowid();
            if let Some((x, y)) = position {
                c.execute(
                    "INSERT INTO node_positions (spatial_entity_id, canvas_id, x, y, radius, pinned, source,
                        layout_version, updated_at_unix_ms) VALUES (?, ?, ?, ?, 4.0, 1, 'manual', 1, ?)",
                    &[spatial.into(), canvas.into(), x.into(), y.into(), now.into()],
                )?;
            }
            let mut ins = c.prepare(
                "INSERT OR IGNORE INTO constellation_members (constellation_id, file_entity_id, added_at_unix_ms) VALUES (?, ?, ?)",
            )?;
            for m in members {
                ins.run(&[id.into(), (*m).into(), now.into()])?;
            }
            Ok(id)
        })?;
        drop(conn);
        self.constellation(id)
    }

    /// Renames, recolours, adds/removes members, soft-deletes or restores a constellation.
    #[allow(clippy::too_many_arguments)]
    pub fn update_constellation(
        &self,
        id: i64,
        name: Option<&str>,
        color: Option<&str>,
        add: &[EntityId],
        remove: &[EntityId],
        deleted: Option<bool>,
    ) -> Result<Constellation, DbError> {
        self.constellation(id)?;
        if let Some(n) = name {
            if n.trim().is_empty() || n.chars().count() > 120 {
                return Err(DbError::Invalid(
                    "nome della costellazione non valido".to_string(),
                ));
            }
        }
        if let Some(c) = color {
            if !is_valid_color(c) {
                return Err(DbError::Invalid(
                    "colore non valido (atteso #rrggbb)".to_string(),
                ));
            }
        }
        for m in add {
            if !self.entity_exists(*m)? {
                return Err(DbError::NotFound(format!("entità {}", m)));
            }
        }
        let now = now_ms();
        let conn = self.connect()?;
        conn.transaction(|c| {
            if let Some(n) = name {
                c.execute("UPDATE constellations SET name = ? WHERE id = ?", &[n.trim().into(), id.into()])?;
            }
            if let Some(col) = color {
                c.execute("UPDATE constellations SET color = ? WHERE id = ?", &[col.into(), id.into()])?;
            }
            for m in add {
                c.execute(
                    "INSERT OR IGNORE INTO constellation_members (constellation_id, file_entity_id, added_at_unix_ms) VALUES (?, ?, ?)",
                    &[id.into(), (*m).into(), now.into()],
                )?;
            }
            for m in remove {
                c.execute(
                    "DELETE FROM constellation_members WHERE constellation_id = ? AND file_entity_id = ?",
                    &[id.into(), (*m).into()],
                )?;
            }
            match deleted {
                Some(true) => {
                    c.execute(
                        "UPDATE constellations SET deleted_at_unix_ms = ? WHERE id = ?",
                        &[now.into(), id.into()],
                    )?;
                }
                Some(false) => {
                    c.execute(
                        "UPDATE constellations SET deleted_at_unix_ms = NULL WHERE id = ?",
                        &[id.into()],
                    )?;
                }
                None => {}
            }
            c.execute("UPDATE constellations SET updated_at_unix_ms = ? WHERE id = ?", &[now.into(), id.into()])?;
            Ok(())
        })?;
        drop(conn);
        self.constellation(id)
    }

    pub fn constellation(&self, id: i64) -> Result<Constellation, DbError> {
        self.list_constellations(true)?
            .into_iter()
            .find(|c| c.id == id)
            .ok_or_else(|| DbError::NotFound(format!("costellazione {}", id)))
    }

    pub fn list_constellations(
        &self,
        include_deleted: bool,
    ) -> Result<Vec<Constellation>, DbError> {
        let conn = self.connect()?;
        let mut st = conn.prepare(
            "SELECT c.id, c.name, c.color, c.note, c.created_at_unix_ms, c.updated_at_unix_ms,
                    c.deleted_at_unix_ms, p.x, p.y
             FROM constellations c
             LEFT JOIN spatial_entities s ON s.constellation_id = c.id
             LEFT JOIN node_positions p ON p.spatial_entity_id = s.id
             ORDER BY c.id",
        )?;
        let mut out = Vec::new();
        while st.step()? {
            let deleted = st.col_opt_i64(6).is_some();
            if deleted && !include_deleted {
                continue;
            }
            let has_pos = st.col_opt_i64(7).is_some();
            out.push(Constellation {
                id: st.col_i64(0),
                name: st.col_text(1),
                color: st.col_opt_text(2),
                note: st.col_opt_text(3),
                members: Vec::new(),
                created_at_unix_ms: st.col_i64(4),
                updated_at_unix_ms: st.col_i64(5),
                deleted,
                x: has_pos.then(|| st.col_f64(7)),
                y: has_pos.then(|| st.col_f64(8)),
            });
        }
        drop(st);
        let mut mem = conn.prepare(
            "SELECT constellation_id, file_entity_id FROM constellation_members ORDER BY constellation_id, file_entity_id",
        )?;
        let mut map: HashMap<i64, Vec<EntityId>> = HashMap::new();
        while mem.step()? {
            map.entry(mem.col_i64(0)).or_default().push(mem.col_i64(1));
        }
        for c in &mut out {
            c.members = map.remove(&c.id).unwrap_or_default();
        }
        Ok(out)
    }

    // ============================================================================================
    // Preferences and status
    // ============================================================================================

    pub fn set_preference(&self, key: &str, value: &serde_json::Value) -> Result<(), DbError> {
        let conn = self.connect()?;
        let json = value.to_string();
        conn.execute(
            "INSERT INTO user_preferences (key, value_json, updated_at_unix_ms) VALUES (?, ?, ?)
             ON CONFLICT(key) DO UPDATE SET value_json = excluded.value_json, updated_at_unix_ms = excluded.updated_at_unix_ms",
            &[key.into(), json.as_str().into(), now_ms().into()],
        )?;
        Ok(())
    }

    pub fn preference(&self, key: &str) -> Result<Option<serde_json::Value>, DbError> {
        let conn = self.connect()?;
        let mut st = conn.prepare("SELECT value_json FROM user_preferences WHERE key = ?")?;
        st.bind_all(&[key.into()])?;
        if st.step()? {
            Ok(serde_json::from_str(&st.col_text(0)).ok())
        } else {
            Ok(None)
        }
    }

    pub fn status(&self) -> Result<StoreStatus, DbError> {
        let conn = self.connect()?;
        let q = |sql: &str| -> Result<u64, DbError> {
            Ok(conn.query_i64(sql, &[])?.unwrap_or(0) as u64)
        };
        let mut s = StoreStatus {
            db_path: self.db_path.display().to_string(),
            schema_version: conn.query_i64("PRAGMA user_version", &[])?.unwrap_or(0),
            snapshots: q("SELECT COUNT(*) FROM snapshots")?,
            entities_present: q("SELECT COUNT(*) FROM file_entities WHERE state = 'present'")?,
            entities_unobserved: q("SELECT COUNT(*) FROM file_entities WHERE state = 'unobserved'")?,
            entities_probably_deleted: q("SELECT COUNT(*) FROM file_entities WHERE state = 'probably_deleted'")?,
            manual_edges: q("SELECT COUNT(*) FROM edges WHERE origin = 'manual' AND state = 'active'")?,
            hidden_edges: q("SELECT COUNT(*) FROM edges WHERE state = 'hidden'")?,
            stale_edges: q("SELECT COUNT(*) FROM edges WHERE state = 'stale'")?,
            fingerprints: q("SELECT COUNT(*) FROM fingerprints")?,
            verified_hashes: q("SELECT COUNT(*) FROM fingerprints WHERE full_hash IS NOT NULL OR sample_covers_full_file = 1")?,
            positions: q("SELECT COUNT(*) FROM node_positions")?,
            pinned_positions: q("SELECT COUNT(*) FROM node_positions WHERE pinned = 1")?,
            constellations: q("SELECT COUNT(*) FROM constellations WHERE deleted_at_unix_ms IS NULL")?,
            ..Default::default()
        };
        let mut st = conn.prepare(
            "SELECT kind, COUNT(*) FROM edges WHERE state = 'active' GROUP BY kind ORDER BY kind",
        )?;
        while st.step()? {
            s.edges_by_kind.insert(st.col_text(0), st.col_i64(1) as u64);
        }
        Ok(s)
    }
}

fn is_valid_color(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit())
}

/// Row of a snapshot used by the history diff.
#[derive(Debug, Clone)]
pub(crate) struct SnapshotRow {
    pub path_display: String,
    pub kind: EntryType,
    pub apparent_bytes: u64,
    pub mtime_sec: Option<i64>,
    pub mtime_nsec: Option<u32>,
    pub access_status: AccessStatus,
    pub identity_match: IdentityMatch,
}

const SNAPSHOT_SELECT: &str = "SELECT id, root_path_raw, root_path_display, label, created_at_unix_ms,
    scan_duration_ms, status, total_apparent_bytes, total_allocated_bytes, total_files_count,
    total_dirs_count, permission_denied_count, io_errors_count, excluded_count, new_entities_count,
    matched_entities_count, moved_entities_count, ambiguous_entries_count, unobserved_entities_count,
    probably_deleted_entities_count FROM snapshots";

fn read_snapshot_header(st: &crate::sqlite::Stmt<'_>) -> SnapshotHeader {
    SnapshotHeader {
        id: st.col_i64(0),
        root_path_raw: st.col_blob(1),
        root_path_display: st.col_text(2),
        label: st.col_opt_text(3),
        created_at_unix_ms: st.col_i64(4),
        scan_duration_ms: st.col_i64(5) as u64,
        status: st.col_text(6),
        total_apparent_bytes: st.col_i64(7) as u64,
        total_allocated_bytes: st.col_i64(8) as u64,
        total_files_count: st.col_i64(9) as u64,
        total_dirs_count: st.col_i64(10) as u64,
        permission_denied_count: st.col_i64(11) as u64,
        io_errors_count: st.col_i64(12) as u64,
        excluded_count: st.col_i64(13) as u64,
        new_entities_count: st.col_i64(14) as u64,
        matched_entities_count: st.col_i64(15) as u64,
        moved_entities_count: st.col_i64(16) as u64,
        ambiguous_entries_count: st.col_i64(17) as u64,
        unobserved_entities_count: st.col_i64(18) as u64,
        probably_deleted_entities_count: st.col_i64(19) as u64,
    }
}

fn read_edge(st: &crate::sqlite::Stmt<'_>) -> Edge {
    Edge {
        id: Some(st.col_i64(0)),
        src: st.col_i64(1),
        dst: st.col_i64(2),
        kind: RelationKind::from_db(&st.col_text(3)).unwrap_or(RelationKind::ManualAssociation),
        origin: EdgeOrigin::from_db(&st.col_text(4)),
        weight: st.col_f64(5),
        confidence: st.col_f64(6),
        provenance: st.col_text(7),
        reasons: serde_json::from_str(&st.col_text(8)).unwrap_or_default(),
        state: EdgeState::from_db(&st.col_text(9)),
    }
}

/// The mutable columns of a `file_entities` row, hashed to detect no-op updates.
struct RowSig<'a> {
    kind: &'a str,
    path: &'a [u8],
    name: &'a str,
    parent: Option<i64>,
    depth: i64,
    dev: Option<i64>,
    ino: Option<i64>,
    apparent: i64,
    allocated: i64,
    subtree_apparent: i64,
    subtree_files: i64,
    mtime_sec: Option<i64>,
    mtime_nsec: Option<i64>,
    nlink: i64,
    hardlink_dup: bool,
    access: &'a str,
}

impl RowSig<'_> {
    fn hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (
            self.kind,
            self.path,
            self.name,
            self.parent,
            self.depth,
            self.dev,
            self.ino,
        )
            .hash(&mut h);
        (
            self.apparent,
            self.allocated,
            self.subtree_apparent,
            self.subtree_files,
        )
            .hash(&mut h);
        (
            self.mtime_sec,
            self.mtime_nsec,
            self.nlink,
            self.hardlink_dup,
            self.access,
        )
            .hash(&mut h);
        h.finish()
    }
}

/// Upper bound on the size of a "changed together" set used for co-activity: larger sets are bulk
/// operations and would produce a quadratic number of meaningless pairs.
const CO_ACTIVITY_MAX_SET: usize = 64;

fn write_snapshot(
    c: &Conn,
    scan: &ScanResult,
    label: Option<&str>,
    known: &[KnownEntity],
    res: &IdentityResolution,
) -> Result<i64, DbError> {
    let now = now_ms();
    c.execute(
        "INSERT INTO snapshots (root_path_raw, root_path_display, label, created_at_unix_ms, scan_duration_ms,
            status, root_dev, cross_mounts, follow_symlinks, total_apparent_bytes, total_allocated_bytes,
            total_files_count, total_dirs_count, total_symlinks_count, permission_denied_count,
            io_errors_count, vanished_count, excluded_count)
         VALUES (?, ?, ?, ?, ?, 'saving', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            scan.root_path_raw.as_slice().into(),
            scan.root_path_display.as_str().into(),
            label.into(),
            now.into(),
            (scan.duration_ms as i64).into(),
            (scan.root_dev as i64).into(),
            scan.config.cross_mounts.into(),
            scan.config.follow_symlinks.into(),
            (scan.total_apparent_bytes as i64).into(),
            (scan.total_allocated_bytes as i64).into(),
            (scan.total_files_count as i64).into(),
            (scan.total_dirs_count as i64).into(),
            (scan.total_symlinks_count as i64).into(),
            (scan.permission_denied_count as i64).into(),
            (scan.io_errors_count as i64).into(),
            (scan.vanished_count as i64).into(),
            (scan.excluded_mounts_count as i64).into(),
        ],
    )?;
    let snapshot_id = c.last_insert_rowid();

    let known_by_id: HashMap<EntityId, &KnownEntity> = known.iter().map(|k| (k.id, k)).collect();
    let known_by_path: HashMap<&[u8], EntityId> = known
        .iter()
        .map(|k| (k.path_raw.as_slice(), k.id))
        .collect();

    let mut ins_entity = c.prepare(
        "INSERT INTO file_entities (kind, state, first_seen_snapshot_id, last_seen_snapshot_id, last_path_raw,
            last_path_display, last_name_display, parent_entity_id, depth, last_dev, last_ino,
            last_apparent_bytes, last_allocated_bytes, subtree_apparent_bytes, subtree_files_count,
            last_mtime_sec, last_mtime_nsec, nlink, is_hardlink_duplicate, access_status,
            created_at_unix_ms, state_changed_at_unix_ms)
         VALUES (?, 'present', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    let mut upd_entity = c.prepare(
        "UPDATE file_entities SET state = 'present', last_seen_snapshot_id = ?, last_path_raw = ?,
            last_path_display = ?, last_name_display = ?, parent_entity_id = ?, depth = ?, last_dev = ?,
            last_ino = ?, last_apparent_bytes = ?, last_allocated_bytes = ?, subtree_apparent_bytes = ?,
            subtree_files_count = ?, last_mtime_sec = ?, last_mtime_nsec = ?, nlink = ?,
            is_hardlink_duplicate = ?, access_status = ?,
            state_changed_at_unix_ms = CASE WHEN state = 'present' THEN state_changed_at_unix_ms ELSE ? END
         WHERE id = ?",
    )?;
    let mut touch_entity =
        c.prepare("UPDATE file_entities SET last_seen_snapshot_id = ? WHERE id = ?")?;
    let mut ins_entry = c.prepare(
        "INSERT INTO snapshot_entries (snapshot_id, entity_id, path_raw, path_display, name_display,
            is_valid_utf8, depth, entry_type, apparent_bytes, allocated_bytes, subtree_apparent_bytes,
            subtree_allocated_bytes, subtree_files_count, dev, ino, nlink, is_hardlink_duplicate,
            mtime_sec, mtime_nsec, access_status, error_message, identity_match, identity_confidence)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    let mut ins_candidate = c.prepare(
        "INSERT INTO identity_candidates (snapshot_id, entity_id, candidate_entity_id, confidence, reason)
         VALUES (?, ?, ?, ?, ?)",
    )?;

    let root_raw = scan.root_path_raw.as_slice();
    let root_parent_entity = parent_raw(root_raw).and_then(|p| known_by_path.get(p).copied());
    let mut entity_of: Vec<EntityId> = vec![0; scan.entries.len()];
    let (mut new_count, mut matched, mut moved, mut ambiguous) = (0u64, 0u64, 0u64, 0u64);
    let mut changed_files: Vec<EntityId> = Vec::new();

    for (idx, e) in scan.entries.iter().enumerate() {
        let a = &res.assignments[idx];
        let abs = absolute_path_raw(root_raw, &e.rel_path_raw);
        let abs_display = String::from_utf8_lossy(&abs).into_owned();
        let parent_entity = match e.parent_id {
            Some(pid) => Some(entity_of[pid.0]),
            None => root_parent_entity,
        };
        let has_meta = e.ino != 0;
        let dev: Value = if has_meta {
            (e.dev as i64).into()
        } else {
            Value::Null
        };
        let ino: Value = if has_meta {
            (e.ino as i64).into()
        } else {
            Value::Null
        };
        let name = if e.depth == 0 {
            String::from_utf8_lossy(crate::identity::basename_raw(root_raw)).into_owned()
        } else {
            e.name_display.clone()
        };
        let has_meta_i = |v: u64| if has_meta { Some(v as i64) } else { None };
        let signature = RowSig {
            kind: e.entry_type.as_db(),
            path: &abs,
            name: &name,
            parent: parent_entity,
            depth: i64::from(e.depth),
            dev: has_meta_i(e.dev),
            ino: has_meta_i(e.ino),
            apparent: e.apparent_bytes as i64,
            allocated: e.allocated_bytes as i64,
            subtree_apparent: e.subtree_apparent_bytes as i64,
            subtree_files: e.subtree_files_count as i64,
            mtime_sec: e.mtime_sec,
            mtime_nsec: e.mtime_nsec.map(i64::from),
            nlink: e.nlink as i64,
            hardlink_dup: e.is_hardlink_duplicate,
            access: e.access_status.as_db(),
        }
        .hash();
        let unchanged = a
            .entity_id
            .and_then(|id| known_by_id.get(&id))
            .map(|k| k.state == EntityState::Present && k.row_signature == signature)
            .unwrap_or(false);
        let entity_id = match a.entity_id {
            Some(id) if unchanged => {
                // Nothing but the "last seen" marker changes: skip rewriting indexed columns.
                touch_entity.run(&[snapshot_id.into(), id.into()])?;
                matched += 1;
                id
            }
            Some(id) => {
                if let Some(k) = known_by_id.get(&id) {
                    if e.entry_type == EntryType::File
                        && (k.apparent_bytes != e.apparent_bytes
                            || k.mtime_sec != e.mtime_sec
                            || k.mtime_nsec != e.mtime_nsec)
                    {
                        changed_files.push(id);
                    }
                }
                upd_entity.run(&[
                    snapshot_id.into(),
                    abs.as_slice().into(),
                    abs_display.as_str().into(),
                    name.as_str().into(),
                    parent_entity.into(),
                    i64::from(e.depth).into(),
                    dev,
                    ino,
                    (e.apparent_bytes as i64).into(),
                    (e.allocated_bytes as i64).into(),
                    (e.subtree_apparent_bytes as i64).into(),
                    (e.subtree_files_count as i64).into(),
                    e.mtime_sec.into(),
                    e.mtime_nsec.map(i64::from).into(),
                    (e.nlink as i64).into(),
                    e.is_hardlink_duplicate.into(),
                    e.access_status.as_db().into(),
                    now.into(),
                    id.into(),
                ])?;
                matched += 1;
                if a.match_kind.is_move() {
                    moved += 1;
                }
                id
            }
            None => {
                ins_entity.run(&[
                    e.entry_type.as_db().into(),
                    snapshot_id.into(),
                    snapshot_id.into(),
                    abs.as_slice().into(),
                    abs_display.as_str().into(),
                    name.as_str().into(),
                    parent_entity.into(),
                    i64::from(e.depth).into(),
                    dev,
                    ino,
                    (e.apparent_bytes as i64).into(),
                    (e.allocated_bytes as i64).into(),
                    (e.subtree_apparent_bytes as i64).into(),
                    (e.subtree_files_count as i64).into(),
                    e.mtime_sec.into(),
                    e.mtime_nsec.map(i64::from).into(),
                    (e.nlink as i64).into(),
                    e.is_hardlink_duplicate.into(),
                    e.access_status.as_db().into(),
                    now.into(),
                    now.into(),
                ])?;
                new_count += 1;
                c.last_insert_rowid()
            }
        };
        entity_of[idx] = entity_id;
        if !a.candidates.is_empty() {
            ambiguous += 1;
            for cand in &a.candidates {
                ins_candidate.run(&[
                    snapshot_id.into(),
                    entity_id.into(),
                    cand.candidate_entity_id.into(),
                    cand.confidence.into(),
                    cand.reason.as_str().into(),
                ])?;
            }
        }
        ins_entry.run(&[
            snapshot_id.into(),
            entity_id.into(),
            abs.as_slice().into(),
            abs_display.as_str().into(),
            name.as_str().into(),
            e.is_valid_utf8.into(),
            i64::from(e.depth).into(),
            e.entry_type.as_db().into(),
            (e.apparent_bytes as i64).into(),
            (e.allocated_bytes as i64).into(),
            (e.subtree_apparent_bytes as i64).into(),
            (e.subtree_allocated_bytes as i64).into(),
            (e.subtree_files_count as i64).into(),
            dev,
            ino,
            (e.nlink as i64).into(),
            e.is_hardlink_duplicate.into(),
            e.mtime_sec.into(),
            e.mtime_nsec.map(i64::from).into(),
            e.access_status.as_db().into(),
            e.error_message.as_deref().into(),
            a.match_kind.as_db().into(),
            a.confidence.into(),
        ])?;
    }

    // Entities inside the root that were not observed.
    let (mut unobserved, mut probably_deleted) = (0u64, 0u64);
    let mut set_state = c.prepare(
        "UPDATE file_entities SET state = ?, state_changed_at_unix_ms = ? WHERE id = ? AND state <> ?",
    )?;
    for (id, verdict) in &res.absent {
        let prev = known_by_id.get(id).map(|k| k.state);
        let new_state = match verdict {
            AbsenceVerdict::ProbablyDeleted => EntityState::ProbablyDeleted,
            // Weaker evidence never overrides an earlier "probably deleted" verdict.
            AbsenceVerdict::Unobserved(_) if prev == Some(EntityState::ProbablyDeleted) => {
                EntityState::ProbablyDeleted
            }
            AbsenceVerdict::Unobserved(_) => EntityState::Unobserved,
        };
        match new_state {
            EntityState::ProbablyDeleted => probably_deleted += 1,
            _ => unobserved += 1,
        }
        set_state.run(&[
            new_state.as_db().into(),
            now.into(),
            (*id).into(),
            new_state.as_db().into(),
        ])?;
    }

    // Co-activity: files changed together since their previous observation.
    if changed_files.len() >= 2 && changed_files.len() <= CO_ACTIVITY_MAX_SET {
        changed_files.sort_unstable();
        let mut up = c.prepare(
            "INSERT INTO co_activity (entity_a, entity_b, co_changes, last_snapshot_id, updated_at_unix_ms)
             VALUES (?, ?, 1, ?, ?)
             ON CONFLICT(entity_a, entity_b) DO UPDATE SET co_changes = co_changes + 1,
                last_snapshot_id = excluded.last_snapshot_id, updated_at_unix_ms = excluded.updated_at_unix_ms",
        )?;
        for i in 0..changed_files.len() {
            for j in i + 1..changed_files.len() {
                up.run(&[
                    changed_files[i].into(),
                    changed_files[j].into(),
                    snapshot_id.into(),
                    now.into(),
                ])?;
            }
        }
    }

    let final_status = match scan.status {
        ScanStatus::Completed => "completed",
        _ => "partial",
    };
    c.execute(
        "UPDATE snapshots SET status = ?, new_entities_count = ?, matched_entities_count = ?,
            moved_entities_count = ?, ambiguous_entries_count = ?, unobserved_entities_count = ?,
            probably_deleted_entities_count = ? WHERE id = ?",
        &[
            final_status.into(),
            (new_count as i64).into(),
            (matched as i64).into(),
            (moved as i64).into(),
            (ambiguous as i64).into(),
            (unobserved as i64).into(),
            (probably_deleted as i64).into(),
            snapshot_id.into(),
        ],
    )?;
    Ok(snapshot_id)
}
