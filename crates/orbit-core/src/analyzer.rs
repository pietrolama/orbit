//! Scan-level helpers inherited from DiskGarden (sorting, filtering, top-N, extension stats) and
//! the informative ORBIT space report ([`space_report`]).

use crate::graph::Graph;
use crate::model::{
    EntityId, EntityState, EntryId, EntryNode, EntryType, RelationKind, ScanResult, SizeMetric,
};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::HashMap;

/// Field used to sort filesystem entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortField {
    AllocatedBytes,
    ApparentBytes,
    Name,
    FilesCount,
    ModifiedTime,
}

/// Filter criteria for searching and inspecting entries inside a [`ScanResult`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EntryFilter {
    /// Case-insensitive substring matched against `name_display` or `rel_path_display`.
    pub query: Option<String>,
    /// Optional restriction to a specific [`EntryType`].
    pub entry_type: Option<EntryType>,
    /// Minimum effective size in bytes according to `metric`.
    pub min_bytes: Option<u64>,
    /// Metric used for `min_bytes`.
    pub metric: Option<SizeMetric>,
    /// Only return entries with an access error (`PermissionDenied` or `IoError`) or non-zero `subtree_error_count`.
    pub only_errors: bool,
    /// Only return sparse files (`allocated_bytes < apparent_bytes`).
    pub only_sparse: bool,
    /// Only return files with multiple hard links (`nlink > 1`).
    pub only_hardlinks: bool,
    /// Optional ancestor directory restriction (`None` searches the entire scan).
    pub ancestor_id: Option<EntryId>,
}

/// Aggregated statistics for a file extension across a scanned tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionStat {
    pub extension: String,
    pub files_count: u64,
    pub allocated_bytes: u64,
    pub apparent_bytes: u64,
}

/// Children of `dir_id` sorted by `sort_field`.
pub fn children_sorted<'a>(
    scan: &'a ScanResult,
    dir_id: EntryId,
    sort_field: SortField,
    descending: bool,
) -> Vec<&'a EntryNode> {
    let Some(dir_node) = scan.get(dir_id) else {
        return Vec::new();
    };
    let mut children: Vec<&'a EntryNode> = dir_node
        .children_ids
        .iter()
        .filter_map(|&cid| scan.get(cid))
        .collect();

    sort_entry_refs(&mut children, sort_field, descending);
    children
}

/// Returns the top `limit` largest regular files across the entire scan, sorted descending by `metric`.
pub fn top_files(
    scan: &ScanResult,
    limit: usize,
    metric: SizeMetric,
    include_hardlink_duplicates: bool,
) -> Vec<&EntryNode> {
    if limit == 0 {
        return Vec::new();
    }
    let mut files: Vec<&EntryNode> = scan
        .entries
        .iter()
        .filter(|e| {
            e.entry_type == EntryType::File
                && (include_hardlink_duplicates || !e.is_hardlink_duplicate)
        })
        .collect();

    let sort_field = match metric {
        SizeMetric::Allocated => SortField::AllocatedBytes,
        SizeMetric::Apparent => SortField::ApparentBytes,
    };
    sort_entry_refs(&mut files, sort_field, true);
    files.truncate(limit);
    files
}

/// Returns the top `limit` largest directories (excluding the root directory itself)
/// sorted descending by their subtree size under `metric`.
pub fn top_directories(scan: &ScanResult, limit: usize, metric: SizeMetric) -> Vec<&EntryNode> {
    if limit == 0 {
        return Vec::new();
    }
    let mut dirs: Vec<&EntryNode> = scan
        .entries
        .iter()
        .filter(|e| e.entry_type == EntryType::Directory && e.depth > 0)
        .collect();

    let sort_field = match metric {
        SizeMetric::Allocated => SortField::AllocatedBytes,
        SizeMetric::Apparent => SortField::ApparentBytes,
    };
    sort_entry_refs(&mut dirs, sort_field, true);
    dirs.truncate(limit);
    dirs
}

/// Searches and filters entries across the scan, returning results sorted by `sort_field`.
pub fn filter_entries<'a>(
    scan: &'a ScanResult,
    filter: &EntryFilter,
    sort_field: SortField,
    descending: bool,
) -> Vec<&'a EntryNode> {
    let query_lower = filter
        .query
        .as_ref()
        .map(|q| q.trim().to_lowercase())
        .filter(|q| !q.is_empty());
    let metric = filter.metric.unwrap_or(SizeMetric::Allocated);

    let ancestor_prefix: Option<&[u8]> =
        filter
            .ancestor_id
            .and_then(|aid| scan.get(aid))
            .and_then(|anode| {
                if anode.depth == 0 {
                    None
                } else {
                    Some(anode.rel_path_raw.as_slice())
                }
            });

    let mut matched: Vec<&'a EntryNode> = scan
        .entries
        .iter()
        .filter(|node| {
            if node.depth == 0 {
                return false;
            }
            if let Some(prefix) = ancestor_prefix {
                if !is_strict_descendant_raw(&node.rel_path_raw, prefix) {
                    return false;
                }
            }
            if let Some(et) = filter.entry_type {
                if node.entry_type != et {
                    return false;
                }
            }
            if let Some(min_b) = filter.min_bytes {
                if node.effective_bytes(metric) < min_b {
                    return false;
                }
            }
            if filter.only_errors && !node.access_status.is_error() && node.subtree_error_count == 0
            {
                return false;
            }
            if filter.only_sparse && !node.is_sparse() {
                return false;
            }
            if filter.only_hardlinks && (node.entry_type == EntryType::Directory || node.nlink <= 1)
            {
                return false;
            }
            if let Some(ref q) = query_lower {
                let name_matches = node.name_display.to_lowercase().contains(q);
                let path_matches = node.rel_path_display.to_lowercase().contains(q);
                if !name_matches && !path_matches {
                    return false;
                }
            }
            true
        })
        .collect();

    sort_entry_refs(&mut matched, sort_field, descending);
    matched
}

/// Computes file size breakdown by lowercase file extension across all non-duplicate regular files.
pub fn extension_distribution(scan: &ScanResult) -> Vec<ExtensionStat> {
    let mut map: HashMap<String, ExtensionStat> = HashMap::new();

    for node in &scan.entries {
        if node.entry_type != EntryType::File || node.is_hardlink_duplicate {
            continue;
        }
        let ext = extract_extension(&node.name_display);
        let entry = map.entry(ext.clone()).or_insert(ExtensionStat {
            extension: ext,
            files_count: 0,
            allocated_bytes: 0,
            apparent_bytes: 0,
        });
        entry.files_count += 1;
        entry.allocated_bytes = entry.allocated_bytes.saturating_add(node.allocated_bytes);
        entry.apparent_bytes = entry.apparent_bytes.saturating_add(node.apparent_bytes);
    }

    let mut stats: Vec<ExtensionStat> = map.into_values().collect();
    stats.sort_by(|a, b| {
        b.allocated_bytes
            .cmp(&a.allocated_bytes)
            .then_with(|| b.apparent_bytes.cmp(&a.apparent_bytes))
            .then_with(|| a.extension.cmp(&b.extension))
    });
    stats
}

fn is_strict_descendant_raw(path_raw: &[u8], ancestor_raw: &[u8]) -> bool {
    if ancestor_raw.is_empty() {
        return !path_raw.is_empty();
    }
    path_raw.len() > ancestor_raw.len()
        && path_raw.starts_with(ancestor_raw)
        && path_raw[ancestor_raw.len()] == b'/'
}

pub fn sort_entry_refs(entries: &mut [&EntryNode], sort_field: SortField, descending: bool) {
    entries.sort_by(|a, b| {
        let primary = match sort_field {
            SortField::AllocatedBytes => a
                .effective_bytes(SizeMetric::Allocated)
                .cmp(&b.effective_bytes(SizeMetric::Allocated))
                .then_with(|| {
                    a.effective_bytes(SizeMetric::Apparent)
                        .cmp(&b.effective_bytes(SizeMetric::Apparent))
                }),
            SortField::ApparentBytes => a
                .effective_bytes(SizeMetric::Apparent)
                .cmp(&b.effective_bytes(SizeMetric::Apparent))
                .then_with(|| {
                    a.effective_bytes(SizeMetric::Allocated)
                        .cmp(&b.effective_bytes(SizeMetric::Allocated))
                }),
            SortField::Name => a
                .name_display
                .to_lowercase()
                .cmp(&b.name_display.to_lowercase()),
            SortField::FilesCount => a.subtree_files_count.cmp(&b.subtree_files_count),
            SortField::ModifiedTime => a
                .mtime_sec
                .unwrap_or(i64::MIN)
                .cmp(&b.mtime_sec.unwrap_or(i64::MIN))
                .then_with(|| a.mtime_nsec.unwrap_or(0).cmp(&b.mtime_nsec.unwrap_or(0))),
        };

        let ordered = if descending {
            primary.reverse()
        } else {
            primary
        };

        if ordered == Ordering::Equal {
            a.rel_path_raw.cmp(&b.rel_path_raw)
        } else {
            ordered
        }
    });
}

fn extract_extension(name: &str) -> String {
    let trimmed = name.trim_start_matches('.');
    match trimmed.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() && ext.len() <= 16 => ext.to_lowercase(),
        _ => "(nessuna)".to_string(),
    }
}

// ================================================================================================
// ORBIT `Recover`: informative space report on the relational graph (never destructive)
// ================================================================================================

/// A file listed in the space report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpaceItem {
    pub entity_id: EntityId,
    pub name: String,
    pub path: String,
    pub apparent_bytes: u64,
    pub allocated_bytes: u64,
}

/// A group of files with identical (verified) or sample-matching (unverified) content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateGroup {
    pub verified: bool,
    pub members: Vec<SpaceItem>,
    /// Allocated bytes occupied by all members (detected).
    pub detected_bytes: u64,
    /// Bytes that would be freed by keeping one copy. `None` when the content is not verified:
    /// ORBIT does not estimate recoverable space from partial evidence.
    pub recoverable_estimate_bytes: Option<u64>,
}

/// A directory matching a well-known regenerable-cache name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheCandidate {
    pub entity_id: EntityId,
    pub name: String,
    pub path: String,
    /// Apparent bytes of the subtree (detected).
    pub detected_bytes: u64,
    pub files: u64,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpaceReport {
    pub largest_files: Vec<SpaceItem>,
    pub duplicate_groups: Vec<DuplicateGroup>,
    pub caches: Vec<CacheCandidate>,
    pub verified_recoverable_bytes: u64,
    pub unverified_duplicate_detected_bytes: u64,
    pub notes: Vec<String>,
}

const CACHE_DIR_NAMES: &[(&str, &str)] = &[
    (
        ".cache",
        "cache utente XDG: in genere rigenerabile dalle applicazioni",
    ),
    (
        "node_modules",
        "dipendenze npm: rigenerabili con il gestore di pacchetti",
    ),
    (
        "target",
        "possibile output di build Cargo: verificare che sia un progetto Rust",
    ),
    ("__pycache__", "bytecode Python: rigenerato automaticamente"),
    (".gradle", "cache Gradle: rigenerabile"),
    (".npm", "cache npm: rigenerabile"),
    (".thumbnails", "miniature: rigenerate dal file manager"),
    (".pytest_cache", "cache pytest: rigenerabile"),
];

/// Builds the informative space report. No action is ever taken on files.
pub fn space_report(graph: &Graph, limit: usize) -> SpaceReport {
    let item = |id: EntityId| {
        graph.node(id).map(|n| SpaceItem {
            entity_id: id,
            name: n.name.clone(),
            path: n.path_display.clone(),
            apparent_bytes: n.apparent_bytes,
            allocated_bytes: n.allocated_bytes,
        })
    };
    let mut files: Vec<_> = graph
        .nodes()
        .iter()
        .filter(|n| {
            n.kind == EntryType::File && n.state == EntityState::Present && !n.is_hardlink_duplicate
        })
        .collect();
    files.sort_by(|a, b| {
        b.allocated_bytes
            .cmp(&a.allocated_bytes)
            .then(a.entity_id.cmp(&b.entity_id))
    });
    let largest_files = files
        .iter()
        .take(limit)
        .filter_map(|n| item(n.entity_id))
        .collect();

    // Union-find over duplicate edges, separately for verified and sample-only evidence.
    let mut groups = Vec::new();
    let mut verified_total = 0u64;
    let mut unverified_total = 0u64;
    for (kind, verified) in [
        (RelationKind::VerifiedDuplicate, true),
        (RelationKind::FingerprintMatch, false),
    ] {
        let mut parent: HashMap<EntityId, EntityId> = HashMap::new();
        fn find(p: &mut HashMap<EntityId, EntityId>, x: EntityId) -> EntityId {
            let mut r = x;
            while let Some(&q) = p.get(&r) {
                if q == r {
                    break;
                }
                r = q;
            }
            p.insert(x, r);
            r
        }
        for e in graph.edges().iter().filter(|e| e.kind == kind) {
            parent.entry(e.src).or_insert(e.src);
            parent.entry(e.dst).or_insert(e.dst);
            let (a, b) = (find(&mut parent, e.src), find(&mut parent, e.dst));
            if a != b {
                let (lo, hi) = if a < b { (a, b) } else { (b, a) };
                parent.insert(hi, lo);
            }
        }
        let keys: Vec<EntityId> = parent.keys().copied().collect();
        let mut sets: std::collections::BTreeMap<EntityId, Vec<EntityId>> =
            std::collections::BTreeMap::new();
        for k in keys {
            let r = find(&mut parent, k);
            sets.entry(r).or_default().push(k);
        }
        for (_, mut members) in sets {
            members.sort_unstable();
            let items: Vec<SpaceItem> = members.iter().filter_map(|&id| item(id)).collect();
            if items.len() < 2 {
                continue;
            }
            let detected: u64 = items.iter().map(|i| i.allocated_bytes).sum();
            let min_alloc = items.iter().map(|i| i.allocated_bytes).min().unwrap_or(0);
            let recoverable = verified.then(|| detected.saturating_sub(min_alloc));
            if verified {
                verified_total += recoverable.unwrap_or(0);
            } else {
                unverified_total += detected;
            }
            groups.push(DuplicateGroup {
                verified,
                members: items,
                detected_bytes: detected,
                recoverable_estimate_bytes: recoverable,
            });
        }
    }
    groups.sort_by(|a, b| {
        b.verified
            .cmp(&a.verified)
            .then(b.detected_bytes.cmp(&a.detected_bytes))
            .then(a.members[0].entity_id.cmp(&b.members[0].entity_id))
    });
    groups.truncate(limit);

    let mut caches: Vec<CacheCandidate> = graph
        .nodes()
        .iter()
        .filter(|n| n.kind == EntryType::Directory && n.state == EntityState::Present)
        .filter_map(|n| {
            let note = CACHE_DIR_NAMES.iter().find(|(name, _)| *name == n.name)?.1;
            Some(CacheCandidate {
                entity_id: n.entity_id,
                name: n.name.clone(),
                path: n.path_display.clone(),
                detected_bytes: n.subtree_apparent_bytes,
                files: n.subtree_files_count,
                note: note.to_string(),
            })
        })
        .collect();
    // Nested matches (node_modules inside node_modules) are reported once, at the outermost level.
    caches.sort_by(|a, b| a.path.cmp(&b.path));
    let mut outer: Vec<CacheCandidate> = Vec::new();
    for c in caches {
        if !outer
            .iter()
            .any(|o| c.path.starts_with(&format!("{}/", o.path)))
        {
            outer.push(c);
        }
    }
    outer.sort_by(|a, b| {
        b.detected_bytes
            .cmp(&a.detected_bytes)
            .then(a.path.cmp(&b.path))
    });
    outer.truncate(limit);

    SpaceReport {
        largest_files,
        duplicate_groups: groups,
        caches: outer,
        verified_recoverable_bytes: verified_total,
        unverified_duplicate_detected_bytes: unverified_total,
        notes: vec![
            "Report informativo: ORBIT non elimina né modifica file.".to_string(),
            "Spazio rilevato ≠ spazio recuperabile: solo i duplicati verificati (hash completo) hanno una stima di recupero.".to_string(),
            "Gli hard link non sono duplicati: condividono lo stesso inode e non occupano spazio aggiuntivo.".to_string(),
            "Le directory di cache sono indicate per nome; il loro contenuto non è verificato.".to_string(),
        ],
    }
}
