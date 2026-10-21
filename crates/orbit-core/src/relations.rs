//! Relation engine: produces a sparse, explainable set of automatic edges.
//!
//! No step compares every file with every other file. Each signal first generates candidates
//! through an index, then scores only those candidates:
//!
//! | signal              | candidate index                              | bound on edges                 |
//! |---------------------|----------------------------------------------|--------------------------------|
//! | `NameSimilarity`    | hash map *normalised name root → files*      | all pairs for small groups, a chain by mtime for larger ones, none for generic groups |
//! | `TemporalAffinity`  | files sorted by mtime, sliding window        | top-k per node, bursts skipped |
//! | `FingerprintMatch` / `VerifiedDuplicate` | hash map *(size, sample hash)* | star around the lowest id      |
//! | `ExplicitReference` | hash map *absolute path → entity*            | references per file are capped |
//!
//! Overall complexity is `O(N log N)` in the number of present entities. All outputs are sorted
//! and deterministic for identical inputs and configuration. Weights and confidences are
//! heuristics, configurable through [`RelationConfig`]; they are not calibrated probabilities.

use crate::fingerprint::Fingerprint;
use crate::model::{
    Edge, EdgeOrigin, EdgeState, EntityId, EntityState, EntryType, GraphNode, RelationKind,
};
use crate::references::ExtractedReference;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Tunable parameters of the relation engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RelationConfig {
    /// τ in `w = exp(-Δt / τ)`, seconds.
    pub temporal_tau_secs: f64,
    /// Maximum Δt considered at all, seconds.
    pub temporal_window_secs: i64,
    /// Maximum temporal neighbours kept per file.
    pub temporal_max_neighbors: usize,
    /// Minimum temporal weight for an edge.
    pub temporal_min_weight: f64,
    /// A file with more than this many other files modified within ±τ is part of a bulk
    /// operation (archive extraction, clone, copy): its temporal proximity carries no meaning.
    pub temporal_burst_threshold: usize,
    /// Name groups up to this size are linked pairwise; larger groups become a chain by mtime.
    pub name_pairwise_max_group: usize,
    /// Name groups larger than this are considered generic and produce no edges.
    pub name_max_group: usize,
    /// Minimum length (in chars) of a normalised name root.
    pub name_min_root_len: usize,
    /// Relative importance of each kind when combining signals into one score.
    pub kind_weights: BTreeMap<RelationKind, f64>,
}

impl Default for RelationConfig {
    fn default() -> Self {
        let kind_weights = BTreeMap::from([
            (RelationKind::ParentChild, 0.3),
            (RelationKind::TemporalAffinity, 0.5),
            (RelationKind::NameSimilarity, 0.8),
            (RelationKind::FingerprintMatch, 0.85),
            (RelationKind::VerifiedDuplicate, 1.0),
            (RelationKind::ExplicitReference, 0.9),
            (RelationKind::ManualAssociation, 1.0),
            (RelationKind::ProjectMembership, 0.7),
        ]);
        Self {
            temporal_tau_secs: 1800.0,
            temporal_window_secs: 4 * 3600,
            temporal_max_neighbors: 3,
            temporal_min_weight: 0.2,
            temporal_burst_threshold: 60,
            name_pairwise_max_group: 8,
            name_max_group: 150,
            name_min_root_len: 3,
            kind_weights,
        }
    }
}

impl RelationConfig {
    pub fn kind_weight(&self, kind: RelationKind) -> f64 {
        self.kind_weights.get(&kind).copied().unwrap_or(0.5)
    }
}

/// Statistics of a relation computation.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RelationReport {
    pub files_considered: usize,
    pub name_groups: usize,
    pub name_groups_generic_skipped: usize,
    pub temporal_burst_files_skipped: usize,
    pub temporal_untrusted_timestamps: usize,
    pub edges_by_kind: BTreeMap<String, usize>,
}

/// Inputs gathered from the store for one computation.
pub struct RelationInputs<'a> {
    pub nodes: &'a [GraphNode],
    pub fingerprints: &'a [Fingerprint],
    pub references: &'a HashMap<EntityId, Vec<ExtractedReference>>,
    /// Pairs `(a, b)` (a < b) with the number of snapshots in which both changed together.
    pub co_activity: &'a [(EntityId, EntityId, u32)],
    /// Wall clock used to reject timestamps in the future (seconds since epoch).
    pub now_unix_secs: i64,
}

// ------------------------------------------------------------------------------------------------
// Name genealogy
// ------------------------------------------------------------------------------------------------

/// Normalised name root and the version markers that were stripped from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameRoot {
    pub root: String,
    pub markers: Vec<String>,
    pub extension: String,
}

const VERSION_WORDS: &[&str] = &[
    "final",
    "finale",
    "def",
    "definitivo",
    "definitiva",
    "backup",
    "bak",
    "old",
    "copy",
    "copia",
    "corretto",
    "corretta",
    "corrected",
    "draft",
    "bozza",
    "new",
    "nuovo",
    "nuova",
    "orig",
    "original",
    "originale",
    "edit",
    "edited",
    "modificato",
    "tmp",
    "temp",
    "latest",
    "fix",
    "fixed",
    "rivisto",
    "rivista",
    "revised",
    "revisione",
    "rev",
    "ver",
    "version",
    "versione",
    "ultimo",
    "ultima",
    "vecchio",
    "vecchia",
    "prova",
    "test",
    "ok",
];

const GENERIC_ROOTS: &[&str] = &[
    "index",
    "readme",
    "main",
    "lib",
    "mod",
    "init",
    "untitled",
    "senza titolo",
    "image",
    "img",
    "screenshot",
    "schermata",
    "document",
    "documento",
    "file",
    "data",
    "config",
    "setup",
    "license",
    "licence",
    "changelog",
    "makefile",
    "cargo",
    "package",
    "default",
    "style",
    "styles",
    "app",
    "utils",
    "util",
    "types",
    "build",
    "test",
    "tests",
    "notes",
    "note",
    "todo",
    "output",
    "out",
    "log",
    "logs",
    "photo",
    "foto",
    "video",
    "audio",
    "desktop",
    "thumbs",
    "icon",
    "logo",
    "cache",
    "version",
    "settings",
    "manifest",
    "metadata",
];

const COMPRESSION_EXTS: &[&str] = &["gz", "bz2", "xz", "zst", "lz", "lzma"];

fn is_version_token(tok: &str) -> bool {
    if tok.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    if VERSION_WORDS.contains(&tok) {
        return true;
    }
    // v2, v10, ver3, rev4, r2
    for prefix in ["v", "ver", "rev", "r"] {
        if let Some(rest) = tok.strip_prefix(prefix) {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit() || c == '.') {
                return true;
            }
        }
    }
    false
}

/// Computes the normalised root of a file name, or `None` if the root is too short or generic.
pub fn name_root(name: &str, min_len: usize) -> Option<NameRoot> {
    let lower = name.to_lowercase();
    let mut stem = lower.trim_start_matches('.').to_string();
    let mut extension = String::new();
    if let Some((s, ext)) = stem.clone().rsplit_once('.') {
        if !s.is_empty() && ext.len() <= 8 {
            extension = ext.to_string();
            stem = s.to_string();
            if COMPRESSION_EXTS.contains(&ext) {
                if let Some((s2, ext2)) = stem.clone().rsplit_once('.') {
                    if !s2.is_empty() && ext2.len() <= 5 {
                        extension = format!("{}.{}", ext2, ext);
                        stem = s2.to_string();
                    }
                }
            }
        }
    }
    let mut kept: Vec<String> = Vec::new();
    let mut markers: Vec<String> = Vec::new();
    for tok in stem
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        if is_version_token(tok) {
            if !tok.chars().all(|c| c.is_ascii_digit()) {
                markers.push(tok.to_string());
            } else if tok.len() >= 6 {
                markers.push("data".to_string());
            } else {
                markers.push(format!("n.{}", tok));
            }
        } else {
            // Split a trailing version glued to a word: "contrattov2" stays, "report2" -> "report".
            let trimmed = tok.trim_end_matches(|c: char| c.is_ascii_digit());
            if trimmed.len() >= min_len && trimmed.len() < tok.len() {
                markers.push(format!("n.{}", &tok[trimmed.len()..]));
                kept.push(trimmed.to_string());
            } else {
                kept.push(tok.to_string());
            }
        }
    }
    let root = kept.join(" ");
    if root.chars().count() < min_len || GENERIC_ROOTS.contains(&root.as_str()) {
        return None;
    }
    markers.dedup();
    Some(NameRoot {
        root,
        markers,
        extension,
    })
}

fn name_similarity_edges(
    files: &[&GraphNode],
    cfg: &RelationConfig,
    report: &mut RelationReport,
    out: &mut Vec<Edge>,
) {
    let mut groups: BTreeMap<String, Vec<(&GraphNode, NameRoot)>> = BTreeMap::new();
    for n in files {
        if let Some(r) = name_root(&n.name, cfg.name_min_root_len) {
            groups.entry(r.root.clone()).or_default().push((n, r));
        }
    }
    for (root, mut members) in groups {
        if members.len() < 2 {
            continue;
        }
        if members.len() > cfg.name_max_group {
            report.name_groups_generic_skipped += 1;
            continue;
        }
        report.name_groups += 1;
        members.sort_by_key(|(n, _)| (n.mtime_sec.unwrap_or(i64::MIN), n.entity_id));
        let pairwise = members.len() <= cfg.name_pairwise_max_group;
        let mut pairs: Vec<(usize, usize)> = Vec::new();
        if pairwise {
            for i in 0..members.len() {
                for j in i + 1..members.len() {
                    pairs.push((i, j));
                }
            }
        } else {
            for i in 1..members.len() {
                pairs.push((i - 1, i));
            }
        }
        for (i, j) in pairs {
            let (a, ra) = &members[i];
            let (b, rb) = &members[j];
            let mut reasons = vec![format!("radice del nome comune «{}»", root)];
            let mut weight: f64 = 0.55;
            let mut confidence: f64 = 0.5;
            let mut marker_list: Vec<&str> = ra
                .markers
                .iter()
                .chain(rb.markers.iter())
                .map(String::as_str)
                .collect();
            marker_list.sort_unstable();
            marker_list.dedup();
            if !marker_list.is_empty() {
                weight += 0.25;
                confidence = 0.7;
                reasons.push(format!("marcatori di versione: {}", marker_list.join(", ")));
            } else if a.name == b.name {
                reasons.push("stesso nome in directory diverse".to_string());
            }
            if ra.extension == rb.extension {
                weight += 0.1;
            } else {
                reasons.push(format!(
                    "estensioni diverse: {} / {}",
                    if ra.extension.is_empty() {
                        "(nessuna)"
                    } else {
                        &ra.extension
                    },
                    if rb.extension.is_empty() {
                        "(nessuna)"
                    } else {
                        &rb.extension
                    }
                ));
            }
            if a.parent_entity_id == b.parent_entity_id {
                weight += 0.05;
                reasons.push("nella stessa directory".to_string());
            }
            if !pairwise {
                confidence *= 0.75;
                reasons.push(format!(
                    "gruppo numeroso ({} file): collegati solo i vicini in ordine di modifica",
                    members.len()
                ));
            }
            reasons.push(
                "la somiglianza del nome non prova che il contenuto sia equivalente".to_string(),
            );
            out.push(undirected_edge(
                a.entity_id,
                b.entity_id,
                RelationKind::NameSimilarity,
                weight.min(1.0),
                confidence,
                format!("name:root={}", root),
                reasons,
            ));
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Temporal affinity
// ------------------------------------------------------------------------------------------------

fn human_duration(secs: f64) -> String {
    let s = secs.abs();
    if s < 60.0 {
        format!("{:.0} secondi", s)
    } else if s < 3600.0 {
        format!("{:.0} minuti", s / 60.0)
    } else {
        format!("{:.1} ore", s / 3600.0)
    }
}

fn temporal_edges(
    files: &[&GraphNode],
    inputs: &RelationInputs<'_>,
    cfg: &RelationConfig,
    report: &mut RelationReport,
    out: &mut Vec<Edge>,
) {
    // Reject missing, epoch-zero and future timestamps.
    let mut timed: Vec<(f64, &GraphNode)> = Vec::new();
    for n in files {
        match n.mtime_sec {
            Some(s) if s > 0 && s <= inputs.now_unix_secs + 86_400 => {
                let t = s as f64 + f64::from(n.mtime_nsec.unwrap_or(0)) / 1e9;
                timed.push((t, n));
            }
            _ => report.temporal_untrusted_timestamps += 1,
        }
    }
    timed.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.1.entity_id.cmp(&b.1.entity_id))
    });
    let tau = cfg.temporal_tau_secs.max(1.0);
    let window = cfg.temporal_window_secs as f64;

    // Density: number of other files within ±τ (two pointers).
    let mut density = vec![0usize; timed.len()];
    let (mut lo, mut hi) = (0usize, 0usize);
    for i in 0..timed.len() {
        while timed[i].0 - timed[lo].0 > tau {
            lo += 1;
        }
        if hi < i {
            hi = i;
        }
        while hi + 1 < timed.len() && timed[hi + 1].0 - timed[i].0 <= tau {
            hi += 1;
        }
        density[i] = hi - lo;
    }

    // Candidate pairs per node.
    let mut per_node: HashMap<EntityId, Vec<(f64, usize, usize)>> = HashMap::new();
    for i in 0..timed.len() {
        if density[i] > cfg.temporal_burst_threshold {
            report.temporal_burst_files_skipped += 1;
            continue;
        }
        for j in i + 1..timed.len() {
            let dt = timed[j].0 - timed[i].0;
            if dt > window {
                break;
            }
            if density[j] > cfg.temporal_burst_threshold {
                continue;
            }
            let (a, b) = (timed[i].1, timed[j].1);
            // Same directory is already a physical relation; temporal affinity is interesting
            // across directories.
            if a.parent_entity_id == b.parent_entity_id {
                continue;
            }
            let w = (-dt / tau).exp();
            if w < cfg.temporal_min_weight {
                break;
            }
            per_node.entry(a.entity_id).or_default().push((w, i, j));
            per_node.entry(b.entity_id).or_default().push((w, i, j));
        }
    }
    // Keep the union of each node's top-k.
    let mut kept: HashSet<(usize, usize)> = HashSet::new();
    let mut keys: Vec<EntityId> = per_node.keys().copied().collect();
    keys.sort_unstable();
    for k in keys {
        let list = per_node.get_mut(&k).unwrap();
        list.sort_by(|x, y| {
            y.0.partial_cmp(&x.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then((x.1, x.2).cmp(&(y.1, y.2)))
        });
        for &(_, i, j) in list.iter().take(cfg.temporal_max_neighbors) {
            kept.insert((i, j));
        }
    }
    let mut kept: Vec<(usize, usize)> = kept.into_iter().collect();
    kept.sort_unstable();
    for (i, j) in kept {
        let (ta, a) = timed[i];
        let (tb, b) = timed[j];
        let dt = tb - ta;
        let w = (-dt / tau).exp();
        let dens = density[i].max(density[j]) as f64;
        let mut confidence = 0.4 * (1.0 - 0.5 * dens / cfg.temporal_burst_threshold.max(1) as f64);
        let mut reasons = vec![
            format!("modificati a {} di distanza", human_duration(dt)),
            "fonte: data di modifica (mtime)".to_string(),
        ];
        let coarse = a.mtime_nsec.unwrap_or(0) == 0 && b.mtime_nsec.unwrap_or(0) == 0;
        if coarse {
            confidence *= 0.8;
            reasons.push("timestamp senza frazioni di secondo: precisione ridotta".to_string());
        }
        if dens > 0.0 {
            reasons.push(format!(
                "{} altri file modificati nello stesso intervallo",
                dens as usize
            ));
        }
        reasons.push("la sola vicinanza temporale è un indizio debole".to_string());
        out.push(undirected_edge(
            a.entity_id,
            b.entity_id,
            RelationKind::TemporalAffinity,
            w,
            confidence.clamp(0.05, 0.4),
            format!("mtime:dt={:.0}s", dt),
            reasons,
        ));
    }
}

fn co_activity_edges(
    inputs: &RelationInputs<'_>,
    present: &HashSet<EntityId>,
    out: &mut Vec<Edge>,
) {
    for &(a, b, count) in inputs.co_activity {
        if count < 2 || !present.contains(&a) || !present.contains(&b) {
            continue;
        }
        let w = 1.0 - 0.5f64.powi(count as i32);
        out.push(undirected_edge(
            a,
            b,
            RelationKind::TemporalAffinity,
            w,
            (0.3 + 0.05 * f64::from(count)).min(0.6),
            format!("co_activity:snapshots={}", count),
            vec![
                format!("modificati insieme in {} scansioni successive", count),
                "attività ricorrente: indizio di contesto comune, non prova di appartenenza"
                    .to_string(),
            ],
        ));
    }
}

// ------------------------------------------------------------------------------------------------
// Fingerprints and duplicates
// ------------------------------------------------------------------------------------------------

fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", b, UNITS[0])
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

fn fingerprint_edges(
    inputs: &RelationInputs<'_>,
    present: &HashSet<EntityId>,
    out: &mut Vec<Edge>,
) {
    let mut groups: BTreeMap<(u64, &str), Vec<&Fingerprint>> = BTreeMap::new();
    for fp in inputs.fingerprints {
        if present.contains(&fp.entity_id) {
            groups
                .entry((fp.size_bytes, fp.sample_hash.as_str()))
                .or_default()
                .push(fp);
        }
    }
    for ((size, _), mut members) in groups {
        if members.len() < 2 {
            continue;
        }
        members.sort_by_key(|f| f.entity_id);
        let tiny = size < 64;
        // Subgroups proven identical by a full-content hash.
        let mut by_content: BTreeMap<&str, Vec<&Fingerprint>> = BTreeMap::new();
        for f in &members {
            if let Some(h) = f.content_hash() {
                by_content.entry(h).or_default().push(f);
            }
        }
        for (hash, sub) in &by_content {
            if sub.len() < 2 {
                continue;
            }
            let rep = sub[0];
            for f in &sub[1..] {
                let mut reasons = vec![
                    format!("contenuto verificato identico ({})", human_bytes(size)),
                    format!("BLAKE3 completo {}…", &hash[..16.min(hash.len())]),
                ];
                if rep.sample_covers_full_file {
                    reasons.push("file piccolo: il campione copre l'intero contenuto".to_string());
                }
                if tiny {
                    reasons.push(
                        "file molto piccolo: l'identità del contenuto è poco significativa"
                            .to_string(),
                    );
                }
                out.push(undirected_edge(
                    rep.entity_id,
                    f.entity_id,
                    RelationKind::VerifiedDuplicate,
                    1.0,
                    if tiny { 0.6 } else { 0.99 },
                    format!("blake3:full={}", &hash[..16.min(hash.len())]),
                    reasons,
                ));
            }
        }
        // Sample match without a full-content proof on both sides: link to the group's first
        // member. Two files proven *different* by full hashes get no edge.
        let anchor = members[0];
        for f in &members[1..] {
            if anchor.content_hash().is_some() && f.content_hash().is_some() {
                // Equal: covered by VerifiedDuplicate above. Different: proven distinct.
                continue;
            }
            out.push(undirected_edge(
                anchor.entity_id,
                f.entity_id,
                RelationKind::FingerprintMatch,
                0.9,
                if tiny { 0.4 } else { 0.7 },
                format!(
                    "blake3:sample={}",
                    &f.sample_hash[..16.min(f.sample_hash.len())]
                ),
                vec![
                    format!(
                        "stessa dimensione ({}) e stessi campioni iniziale/finale",
                        human_bytes(size)
                    ),
                    "corrispondenza parziale: non è una verifica completa del contenuto"
                        .to_string(),
                ],
            ));
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Explicit references
// ------------------------------------------------------------------------------------------------

fn reference_edges(
    inputs: &RelationInputs<'_>,
    by_path: &HashMap<&[u8], &GraphNode>,
    by_id: &HashMap<EntityId, &GraphNode>,
    out: &mut Vec<Edge>,
) {
    let mut sources: Vec<&EntityId> = inputs.references.keys().collect();
    sources.sort_unstable();
    for src in sources {
        let Some(src_node) = by_id.get(src) else {
            continue;
        };
        for r in &inputs.references[src] {
            let Some(dst) = by_path.get(r.target_path_raw.as_slice()) else {
                continue;
            };
            if dst.entity_id == *src {
                continue;
            }
            let label = match r.syntax.as_str() {
                "markdown_image" => "immagine Markdown",
                "markdown_definition" => "definizione di link Markdown",
                "autolink" => "link file:// ",
                "plain_path" => "percorso nel testo",
                _ => "link Markdown",
            };
            out.push(Edge {
                id: None,
                src: *src,
                dst: dst.entity_id,
                kind: RelationKind::ExplicitReference,
                origin: EdgeOrigin::Auto,
                weight: 0.9,
                confidence: if r.syntax == "plain_path" { 0.75 } else { 0.9 },
                provenance: format!("{}:line={}", r.syntax, r.line),
                reasons: vec![format!(
                    "riferimento trovato in «{}» (riga {}, {})",
                    src_node.name, r.line, label
                )],
                state: EdgeState::Active,
            });
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Assembly
// ------------------------------------------------------------------------------------------------

fn undirected_edge(
    a: EntityId,
    b: EntityId,
    kind: RelationKind,
    weight: f64,
    confidence: f64,
    provenance: String,
    reasons: Vec<String>,
) -> Edge {
    let (src, dst) = if a <= b { (a, b) } else { (b, a) };
    Edge {
        id: None,
        src,
        dst,
        kind,
        origin: EdgeOrigin::Auto,
        weight: weight.clamp(0.0, 1.0),
        confidence: confidence.clamp(0.0, 1.0),
        provenance,
        reasons,
        state: EdgeState::Active,
    }
}

/// Computes all automatic edges for the current state. Deterministic: the output is sorted by
/// `(src, dst, kind)` and contains no self-loops and no duplicate `(src, dst, kind)` keys (when two
/// signals of the same kind produce the same pair, the stronger one is kept).
pub fn compute_auto_edges(
    inputs: &RelationInputs<'_>,
    cfg: &RelationConfig,
) -> (Vec<Edge>, RelationReport) {
    let mut report = RelationReport::default();
    let present_nodes: Vec<&GraphNode> = inputs
        .nodes
        .iter()
        .filter(|n| n.state == EntityState::Present)
        .collect();
    let mut files: Vec<&GraphNode> = present_nodes
        .iter()
        .copied()
        .filter(|n| n.kind == EntryType::File && !n.is_hardlink_duplicate)
        .collect();
    files.sort_by_key(|n| n.entity_id);
    report.files_considered = files.len();
    let present: HashSet<EntityId> = present_nodes.iter().map(|n| n.entity_id).collect();
    let by_path: HashMap<&[u8], &GraphNode> = present_nodes
        .iter()
        .map(|n| (n.path_raw.as_slice(), *n))
        .collect();
    let by_id: HashMap<EntityId, &GraphNode> =
        present_nodes.iter().map(|n| (n.entity_id, *n)).collect();

    let mut edges = Vec::new();
    name_similarity_edges(&files, cfg, &mut report, &mut edges);
    temporal_edges(&files, inputs, cfg, &mut report, &mut edges);
    co_activity_edges(inputs, &present, &mut edges);
    fingerprint_edges(inputs, &present, &mut edges);
    reference_edges(inputs, &by_path, &by_id, &mut edges);

    // Deduplicate by key, keeping the strongest.
    let mut best: BTreeMap<(EntityId, EntityId, RelationKind), Edge> = BTreeMap::new();
    for e in edges {
        if e.src == e.dst {
            continue;
        }
        let key = (e.src, e.dst, e.kind);
        match best.get(&key) {
            Some(cur) if cur.strength() >= e.strength() => {}
            _ => {
                best.insert(key, e);
            }
        }
    }
    let out: Vec<Edge> = best.into_values().collect();
    for e in &out {
        *report
            .edges_by_kind
            .entry(e.kind.as_db().to_string())
            .or_default() += 1;
    }
    (out, report)
}

/// Combines several edges between the same pair into one score with a noisy-or:
/// `1 - Π (1 - kind_weight * weight * confidence)`.
pub fn combined_score<'a>(edges: impl IntoIterator<Item = &'a Edge>, cfg: &RelationConfig) -> f64 {
    let mut keep = 1.0;
    for e in edges {
        if e.state != EdgeState::Active {
            continue;
        }
        let s = (cfg.kind_weight(e.kind) * e.strength()).clamp(0.0, 1.0);
        keep *= 1.0 - s;
    }
    1.0 - keep
}

/// Human-readable label of a relation kind (Italian UI language).
pub fn kind_label(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::ParentChild => "struttura fisica",
        RelationKind::TemporalAffinity => "prossimità temporale",
        RelationKind::NameSimilarity => "genealogia del nome",
        RelationKind::FingerprintMatch => "fingerprint corrispondente",
        RelationKind::VerifiedDuplicate => "duplicato verificato",
        RelationKind::ExplicitReference => "riferimento esplicito",
        RelationKind::ManualAssociation => "associazione manuale",
        RelationKind::ProjectMembership => "costellazione",
    }
}
