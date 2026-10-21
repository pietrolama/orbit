//! History: entity-based comparison of two snapshots.
//!
//! Because observations are linked to persistent entities, the diff can tell a *move* from a
//! delete + create, and a missing file from an unreadable one.

use crate::identity::is_within_raw;
use crate::model::{AccessStatus, EntityId, EntryType, IdentityMatch};
use crate::store::{DbError, OrbitStore, SnapshotHeader, SnapshotRow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MovedItem {
    pub entity_id: EntityId,
    pub kind: EntryType,
    pub from_path: String,
    pub to_path: String,
    pub identity_match: IdentityMatch,
    pub content_changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModifiedItem {
    pub entity_id: EntityId,
    pub path: String,
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub before_mtime_sec: Option<i64>,
    pub after_mtime_sec: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppearedItem {
    pub entity_id: EntityId,
    pub kind: EntryType,
    pub path: String,
    pub apparent_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisappearedItem {
    pub entity_id: EntityId,
    pub kind: EntryType,
    pub path: String,
    /// `probably_deleted`, `unobserved` or `outside_target_root`.
    pub verdict: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryDiff {
    pub base: SnapshotHeader,
    pub target: SnapshotHeader,
    pub moved: Vec<MovedItem>,
    pub modified: Vec<ModifiedItem>,
    pub appeared: Vec<AppearedItem>,
    pub disappeared: Vec<DisappearedItem>,
    pub totals: DiffTotals,
    /// `true` if any list was truncated to `limit` items.
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiffTotals {
    pub moved: usize,
    pub modified: usize,
    pub appeared: usize,
    pub disappeared_probably_deleted: usize,
    pub disappeared_unobserved: usize,
    pub disappeared_outside_root: usize,
}

fn parent_display(path: &str) -> Option<&str> {
    if path.len() <= 1 {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some("/"),
        Some(i) => Some(&path[..i]),
        None => None,
    }
}

/// Compares two snapshots. Lists are sorted by path and truncated to `limit` items each.
pub fn diff_snapshots(
    store: &OrbitStore,
    base_id: i64,
    target_id: i64,
    limit: usize,
) -> Result<HistoryDiff, DbError> {
    let base = store.snapshot_header(base_id)?;
    let target = store.snapshot_header(target_id)?;
    let base_rows = store.snapshot_rows(base_id)?;
    let target_rows = store.snapshot_rows(target_id)?;

    let mut moved = Vec::new();
    let mut modified = Vec::new();
    let mut appeared = Vec::new();
    let mut disappeared = Vec::new();
    let mut totals = DiffTotals::default();

    let changed = |a: &SnapshotRow, b: &SnapshotRow| {
        a.kind == EntryType::File
            && (a.apparent_bytes != b.apparent_bytes
                || a.mtime_sec != b.mtime_sec
                || a.mtime_nsec != b.mtime_nsec)
    };

    for (id, t) in &target_rows {
        match base_rows.get(id) {
            Some(b) => {
                if b.path_display != t.path_display {
                    moved.push(MovedItem {
                        entity_id: *id,
                        kind: t.kind,
                        from_path: b.path_display.clone(),
                        to_path: t.path_display.clone(),
                        identity_match: t.identity_match,
                        content_changed: changed(b, t),
                    });
                } else if changed(b, t) {
                    modified.push(ModifiedItem {
                        entity_id: *id,
                        path: t.path_display.clone(),
                        before_bytes: b.apparent_bytes,
                        after_bytes: t.apparent_bytes,
                        before_mtime_sec: b.mtime_sec,
                        after_mtime_sec: t.mtime_sec,
                    });
                }
            }
            None => appeared.push(AppearedItem {
                entity_id: *id,
                kind: t.kind,
                path: t.path_display.clone(),
                apparent_bytes: t.apparent_bytes,
            }),
        }
    }

    let target_paths: HashMap<&str, AccessStatus> = target_rows
        .values()
        .map(|r| (r.path_display.as_str(), r.access_status))
        .collect();
    let target_complete = target.status == "completed";

    for (id, b) in &base_rows {
        if target_rows.contains_key(id) {
            continue;
        }
        let (verdict, reason) = if !is_within_raw(
            b.path_display.as_bytes(),
            target.root_path_display.as_bytes(),
        ) {
            totals.disappeared_outside_root += 1;
            (
                "outside_target_root",
                "il percorso non rientra nella radice dello snapshot di destinazione".to_string(),
            )
        } else {
            let mut cur = parent_display(&b.path_display);
            let mut found: Option<AccessStatus> = None;
            while let Some(p) = cur {
                if let Some(s) = target_paths.get(p) {
                    found = Some(*s);
                    break;
                }
                cur = parent_display(p);
            }
            match found {
                Some(AccessStatus::Ok) if target_complete => {
                    totals.disappeared_probably_deleted += 1;
                    (
                        "probably_deleted",
                        "non trovato da una scansione completa della directory che lo conteneva"
                            .to_string(),
                    )
                }
                Some(AccessStatus::Ok) => {
                    totals.disappeared_unobserved += 1;
                    (
                        "unobserved",
                        "lo snapshot di destinazione è parziale: l'assenza non è dimostrata"
                            .to_string(),
                    )
                }
                Some(_) => {
                    totals.disappeared_unobserved += 1;
                    (
                        "unobserved",
                        "una directory antenata non era leggibile o era esclusa".to_string(),
                    )
                }
                None => {
                    totals.disappeared_unobserved += 1;
                    (
                        "unobserved",
                        "nessun antenato osservato nello snapshot di destinazione".to_string(),
                    )
                }
            }
        };
        disappeared.push(DisappearedItem {
            entity_id: *id,
            kind: b.kind,
            path: b.path_display.clone(),
            verdict: verdict.to_string(),
            reason,
        });
    }

    totals.moved = moved.len();
    totals.modified = modified.len();
    totals.appeared = appeared.len();

    moved.sort_by(|a, b| a.to_path.cmp(&b.to_path));
    modified.sort_by(|a, b| a.path.cmp(&b.path));
    appeared.sort_by(|a, b| a.path.cmp(&b.path));
    disappeared.sort_by(|a, b| a.path.cmp(&b.path));
    let truncated = [
        moved.len(),
        modified.len(),
        appeared.len(),
        disappeared.len(),
    ]
    .iter()
    .any(|&n| n > limit);
    moved.truncate(limit);
    modified.truncate(limit);
    appeared.truncate(limit);
    disappeared.truncate(limit);

    Ok(HistoryDiff {
        base,
        target,
        moved,
        modified,
        appeared,
        disappeared,
        totals,
        truncated,
    })
}

/// Resolves `latest` / `previous` / numeric snapshot references. `previous` is the snapshot
/// before `latest` with the same root.
pub fn resolve_snapshot_ref(store: &OrbitStore, reference: &str) -> Result<i64, DbError> {
    let snaps: Vec<SnapshotHeader> = store
        .list_snapshots()?
        .into_iter()
        .filter(|s| s.status == "completed" || s.status == "partial")
        .collect();
    match reference {
        "latest" => snaps
            .first()
            .map(|s| s.id)
            .ok_or_else(|| DbError::NotFound("nessuno snapshot disponibile".to_string())),
        "previous" => {
            let latest = snaps
                .first()
                .ok_or_else(|| DbError::NotFound("nessuno snapshot disponibile".to_string()))?;
            snaps
                .iter()
                .skip(1)
                .find(|s| s.root_path_raw == latest.root_path_raw)
                .map(|s| s.id)
                .ok_or_else(|| {
                    DbError::NotFound(
                        "nessuno snapshot precedente per la stessa radice".to_string(),
                    )
                })
        }
        other => other
            .parse::<i64>()
            .map_err(|_| DbError::Invalid(format!("riferimento snapshot non valido: {}", other))),
    }
}
