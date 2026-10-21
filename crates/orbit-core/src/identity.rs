//! Best-effort persistent identity across scans.
//!
//! A path is *where* a file is; an inode is *how the filesystem currently names it*. Neither is an
//! eternal identity: editors replace files atomically (new inode, same path), `mv` keeps the inode
//! but changes the path, cross-filesystem moves change both, and inodes are reused after deletion.
//!
//! [`resolve_identities`] links the observations of a new scan to known entities in stages, from
//! the strongest evidence to the weakest, and records only *plausible* matches (as candidates)
//! when the evidence is not sufficient to merge. It never invents a certain match.
//!
//! | stage | evidence                                         | result                      | confidence |
//! |-------|--------------------------------------------------|-----------------------------|-----------:|
//! | 1     | same absolute path, same kind, same inode        | `SamePath`                  | 1.00 |
//! | 1     | same absolute path, same kind, different inode   | `SamePathReplaced`          | 0.85 |
//! | 2     | same (dev, ino), same kind, same size and mtime  | `Moved`                     | 0.95 |
//! | 2     | same (dev, ino), directory                       | `Moved`                     | 0.90 |
//! | 2     | same (dev, ino), same name, metadata changed     | `MovedModified`             | 0.75 |
//! | 2     | same (dev, ino), different name and metadata     | candidate only (inode reuse) | 0.35 |
//! | 3     | unique (name, size, mtime) among missing files   | `MovedByContentMetadata`    | 0.70 |
//! | 3     | several (name, size, mtime) matches              | candidates only             | 0.40 |
//!
//! Known entities that were not matched are classified with [`AbsenceVerdict`]: an entity is
//! `ProbablyDeleted` only when the scan completed and its nearest observed ancestor was read
//! successfully; otherwise it is merely `Unobserved`.

use crate::model::{
    AccessStatus, EntityId, EntityState, EntryType, IdentityMatch, ScanResult, ScanStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// A persistent entity as known before the new scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownEntity {
    pub id: EntityId,
    pub kind: EntryType,
    pub state: EntityState,
    /// Absolute raw path at the last observation.
    pub path_raw: Vec<u8>,
    pub dev: Option<u64>,
    pub ino: Option<u64>,
    pub apparent_bytes: u64,
    pub mtime_sec: Option<i64>,
    pub mtime_nsec: Option<u32>,
    pub last_seen_snapshot_id: i64,
    /// Hash of all stored mutable columns, used by the store to skip no-op updates (`0` = unknown).
    pub row_signature: u64,
}

/// A plausible but unproven identity link.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdentityCandidate {
    pub candidate_entity_id: EntityId,
    pub confidence: f64,
    pub reason: String,
}

/// Outcome for one scan entry (indexed like [`ScanResult::entries`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntryAssignment {
    /// Matched entity, or `None` when a new entity must be created.
    pub entity_id: Option<EntityId>,
    pub match_kind: IdentityMatch,
    pub confidence: f64,
    pub candidates: Vec<IdentityCandidate>,
}

/// Why a known entity was not seen in the new scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "verdict", content = "reason")]
pub enum AbsenceVerdict {
    /// The scan could not prove absence (reason given).
    Unobserved(String),
    /// A complete scan read the nearest ancestor successfully and did not find the entity.
    ProbablyDeleted,
}

/// Complete identity resolution for one scan.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityResolution {
    pub assignments: Vec<EntryAssignment>,
    /// Known entities inside the scanned root that were not matched.
    pub absent: Vec<(EntityId, AbsenceVerdict)>,
}

impl IdentityResolution {
    pub fn count(&self, pred: impl Fn(&EntryAssignment) -> bool) -> usize {
        self.assignments.iter().filter(|a| pred(a)).count()
    }
}

/// Joins the scan root with a relative raw path.
pub fn absolute_path_raw(root_raw: &[u8], rel_raw: &[u8]) -> Vec<u8> {
    if rel_raw.is_empty() {
        return root_raw.to_vec();
    }
    let mut out = Vec::with_capacity(root_raw.len() + 1 + rel_raw.len());
    out.extend_from_slice(root_raw);
    if !root_raw.ends_with(b"/") {
        out.push(b'/');
    }
    out.extend_from_slice(rel_raw);
    out
}

/// `true` if `path` equals `root` or lies strictly below it (byte-wise, component aware).
pub fn is_within_raw(path: &[u8], root: &[u8]) -> bool {
    if path == root {
        return true;
    }
    if root == b"/" {
        return path.starts_with(b"/");
    }
    path.len() > root.len() && path.starts_with(root) && path[root.len()] == b'/'
}

/// Parent of an absolute raw path (`None` for `/`).
pub fn parent_raw(path: &[u8]) -> Option<&[u8]> {
    if path.len() <= 1 {
        return None;
    }
    let idx = path.iter().rposition(|&b| b == b'/')?;
    if idx == 0 {
        Some(b"/")
    } else {
        Some(&path[..idx])
    }
}

/// Last component of a raw path.
pub fn basename_raw(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(idx) => &path[idx + 1..],
        None => path,
    }
}

fn has_inode(dev: Option<u64>, ino: Option<u64>) -> Option<(u64, u64)> {
    match (dev, ino) {
        (Some(d), Some(i)) if i != 0 => Some((d, i)),
        _ => None,
    }
}

/// Resolves the identities of all entries of `scan` against `known` entities.
///
/// `known` may contain entities from anywhere; only those inside the scanned root (or not
/// currently `Present`) are eligible for move detection, so that two simultaneously existing
/// copies in different roots are never merged.
pub fn resolve_identities(scan: &ScanResult, known: &[KnownEntity]) -> IdentityResolution {
    let root_raw = scan.root_path_raw.as_slice();
    let n = scan.entries.len();
    let abs_paths: Vec<Vec<u8>> = scan
        .entries
        .iter()
        .map(|e| absolute_path_raw(root_raw, &e.rel_path_raw))
        .collect();

    let mut assignments: Vec<EntryAssignment> = vec![
        EntryAssignment {
            entity_id: None,
            match_kind: IdentityMatch::New,
            confidence: 1.0,
            candidates: Vec::new(),
        };
        n
    ];
    let mut consumed: HashSet<EntityId> = HashSet::new();

    // Index known entities by path. Several entities may share a path (a file deleted long ago and
    // recreated): prefer the most recently seen, then the lowest id for determinism.
    let mut by_path: HashMap<&[u8], &KnownEntity> = HashMap::new();
    for k in known {
        by_path
            .entry(k.path_raw.as_slice())
            .and_modify(|cur| {
                if (k.last_seen_snapshot_id, -k.id) > (cur.last_seen_snapshot_id, -cur.id) {
                    *cur = k;
                }
            })
            .or_insert(k);
    }

    // ---- Stage 1: same path ------------------------------------------------------------------
    for (idx, entry) in scan.entries.iter().enumerate() {
        let Some(k) = by_path.get(abs_paths[idx].as_slice()) else {
            continue;
        };
        if k.kind != entry.entry_type || consumed.contains(&k.id) {
            continue;
        }
        let entry_inode = has_inode(Some(entry.dev), Some(entry.ino));
        let known_inode = has_inode(k.dev, k.ino);
        let (kind, conf) = match (entry_inode, known_inode) {
            (Some(a), Some(b)) if a == b => (IdentityMatch::SamePath, 1.0),
            (Some(_), Some(_)) => (IdentityMatch::SamePathReplaced, 0.85),
            // Metadata unavailable on one side (lstat error): the path is the only evidence.
            _ => (IdentityMatch::SamePath, 0.8),
        };
        consumed.insert(k.id);
        assignments[idx] = EntryAssignment {
            entity_id: Some(k.id),
            match_kind: kind,
            confidence: conf,
            candidates: Vec::new(),
        };
    }

    // Entities eligible for move detection: not matched by path, and either inside the scanned
    // root (so we know they are gone from their old place) or not currently present elsewhere.
    let eligible: Vec<&KnownEntity> = known
        .iter()
        .filter(|k| !consumed.contains(&k.id))
        .filter(|k| is_within_raw(&k.path_raw, root_raw) || k.state != EntityState::Present)
        .collect();

    // ---- Stage 2: same inode -----------------------------------------------------------------
    let mut by_inode: HashMap<(u64, u64), Vec<&KnownEntity>> = HashMap::new();
    for k in &eligible {
        if let Some(key) = has_inode(k.dev, k.ino) {
            by_inode.entry(key).or_default().push(k);
        }
    }
    for list in by_inode.values_mut() {
        list.sort_by_key(|k| k.id);
    }

    for (idx, entry) in scan.entries.iter().enumerate() {
        if assignments[idx].entity_id.is_some() {
            continue;
        }
        let Some(key) = has_inode(Some(entry.dev), Some(entry.ino)) else {
            continue;
        };
        let Some(list) = by_inode.get(&key) else {
            continue;
        };
        let entry_name = basename_raw(&abs_paths[idx]);
        let available: Vec<&&KnownEntity> = list
            .iter()
            .filter(|k| k.kind == entry.entry_type && !consumed.contains(&k.id))
            .collect();
        if available.is_empty() {
            continue;
        }
        // With several hard-link paths for the same inode, the name disambiguates.
        let pick = available
            .iter()
            .find(|k| basename_raw(&k.path_raw) == entry_name)
            .or_else(|| (available.len() == 1).then(|| &available[0]))
            .copied();

        let Some(k) = pick else {
            assignments[idx].candidates = available
                .iter()
                .map(|k| IdentityCandidate {
                    candidate_entity_id: k.id,
                    confidence: 0.4,
                    reason: "stesso inode condiviso da più percorsi noti (hard link)".to_string(),
                })
                .collect();
            continue;
        };

        let same_name = basename_raw(&k.path_raw) == entry_name;
        let same_meta = k.apparent_bytes == entry.apparent_bytes
            && k.mtime_sec == entry.mtime_sec
            && k.mtime_nsec == entry.mtime_nsec;

        let decision = if entry.entry_type == EntryType::Directory {
            Some((IdentityMatch::Moved, 0.9))
        } else if same_meta {
            Some((IdentityMatch::Moved, 0.95))
        } else if same_name {
            Some((IdentityMatch::MovedModified, 0.75))
        } else {
            None
        };

        match decision {
            Some((kind, conf)) => {
                consumed.insert(k.id);
                assignments[idx] = EntryAssignment {
                    entity_id: Some(k.id),
                    match_kind: kind,
                    confidence: conf,
                    candidates: Vec::new(),
                };
            }
            None => {
                assignments[idx].candidates.push(IdentityCandidate {
                    candidate_entity_id: k.id,
                    confidence: 0.35,
                    reason: "stesso inode ma nome e metadati diversi (possibile riuso dell'inode)"
                        .to_string(),
                });
            }
        }
    }

    // ---- Stage 3: (name, size, mtime) for regular files ----------------------------------------
    type MetaKey<'a> = (&'a [u8], u64, i64, u32);
    let mut by_meta: HashMap<MetaKey<'_>, Vec<&KnownEntity>> = HashMap::new();
    for k in &eligible {
        if k.kind != EntryType::File || consumed.contains(&k.id) || k.apparent_bytes == 0 {
            continue;
        }
        let (Some(ms), Some(mn)) = (k.mtime_sec, k.mtime_nsec) else {
            continue;
        };
        by_meta
            .entry((basename_raw(&k.path_raw), k.apparent_bytes, ms, mn))
            .or_default()
            .push(k);
    }
    // Count scan-side duplicates of each key: a unique match must be unique on both sides.
    let mut scan_side: HashMap<MetaKey<'_>, usize> = HashMap::new();
    let entry_key = |idx: usize| -> Option<MetaKey<'_>> {
        let e = &scan.entries[idx];
        if e.entry_type != EntryType::File || e.apparent_bytes == 0 {
            return None;
        }
        Some((
            basename_raw(&abs_paths[idx]),
            e.apparent_bytes,
            e.mtime_sec?,
            e.mtime_nsec?,
        ))
    };
    for (idx, a) in assignments.iter().enumerate() {
        if a.entity_id.is_none() {
            if let Some(key) = entry_key(idx) {
                *scan_side.entry(key).or_default() += 1;
            }
        }
    }
    #[allow(clippy::needless_range_loop)]
    for idx in 0..n {
        if assignments[idx].entity_id.is_some() {
            continue;
        }
        let Some(key) = entry_key(idx) else {
            continue;
        };
        let Some(list) = by_meta.get(&key) else {
            continue;
        };
        let available: Vec<&&KnownEntity> =
            list.iter().filter(|k| !consumed.contains(&k.id)).collect();
        if available.len() == 1 && scan_side.get(&key) == Some(&1) {
            let k = available[0];
            consumed.insert(k.id);
            assignments[idx] = EntryAssignment {
                entity_id: Some(k.id),
                match_kind: IdentityMatch::MovedByContentMetadata,
                confidence: 0.7,
                candidates: Vec::new(),
            };
        } else {
            for k in available {
                assignments[idx].candidates.push(IdentityCandidate {
                    candidate_entity_id: k.id,
                    confidence: 0.4,
                    reason: "stesso nome, dimensione e data di modifica di un file non più osservato (più corrispondenze possibili)".to_string(),
                });
            }
        }
    }

    // ---- Absence classification ----------------------------------------------------------------
    // Map of observed absolute paths -> access status (to find the nearest observed ancestor).
    let observed: HashMap<&[u8], AccessStatus> = abs_paths
        .iter()
        .zip(scan.entries.iter())
        .map(|(p, e)| (p.as_slice(), e.access_status))
        .collect();

    let mut absent = Vec::new();
    for k in known {
        if consumed.contains(&k.id) || !is_within_raw(&k.path_raw, root_raw) {
            continue;
        }
        let verdict = classify_absence(scan, &observed, &k.path_raw);
        absent.push((k.id, verdict));
    }
    absent.sort_by_key(|(id, _)| *id);

    IdentityResolution {
        assignments,
        absent,
    }
}

fn classify_absence(
    scan: &ScanResult,
    observed: &HashMap<&[u8], AccessStatus>,
    path: &[u8],
) -> AbsenceVerdict {
    match scan.status {
        ScanStatus::Cancelled => {
            return AbsenceVerdict::Unobserved("scansione annullata prima del termine".to_string())
        }
        ScanStatus::Failed => {
            return AbsenceVerdict::Unobserved("scansione fallita".to_string());
        }
        _ => {}
    }
    let mut cur = parent_raw(path);
    while let Some(p) = cur {
        if let Some(status) = observed.get(p) {
            return match status {
                AccessStatus::Ok => AbsenceVerdict::ProbablyDeleted,
                AccessStatus::PermissionDenied => AbsenceVerdict::Unobserved(
                    "una directory antenata non era leggibile (permesso negato)".to_string(),
                ),
                AccessStatus::ExcludedMount => AbsenceVerdict::Unobserved(
                    "una directory antenata è stata esclusa dalla scansione (mount o esclusione configurata)".to_string(),
                ),
                AccessStatus::SymlinkSkipped | AccessStatus::SymlinkCycleSkipped => {
                    AbsenceVerdict::Unobserved(
                        "un antenato è un link simbolico non attraversato".to_string(),
                    )
                }
                AccessStatus::Vanished | AccessStatus::IoError => AbsenceVerdict::Unobserved(
                    "errore di lettura su una directory antenata".to_string(),
                ),
            };
        }
        cur = parent_raw(p);
    }
    AbsenceVerdict::Unobserved("nessun antenato osservato nella scansione".to_string())
}
