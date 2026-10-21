//! Indexing pipeline: scan → snapshot & identities → fingerprints → references → relations →
//! layout. Each stage is a separate, testable module; this file only orchestrates them.

use crate::fingerprint::{refresh_fingerprints, FingerprintBudget, FingerprintReport};
use crate::layout::{update_layout, LayoutConfig, LayoutReport};
use crate::model::ScanProgress;
use crate::model::{EntityState, ScanConfig, ScanResult};
use crate::references::{refresh_references, ReferenceReport};
use crate::relations::{compute_auto_edges, RelationConfig, RelationInputs, RelationReport};
use crate::scanner::{scan_directory_with_progress, CancelToken};
use crate::store::{DbError, EdgeApplyReport, OrbitStore, RecordSummary, MAIN_CANVAS};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

/// Options for one indexing run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexOptions {
    pub fingerprint: FingerprintBudget,
    pub relations: RelationConfig,
    /// Skip relation computation (snapshot only).
    pub skip_relations: bool,
    /// Home directory used to resolve `~/` references (raw bytes).
    #[serde(skip)]
    pub home: Option<Vec<u8>>,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            fingerprint: FingerprintBudget::default(),
            relations: RelationConfig::default(),
            skip_relations: false,
            home: std::env::var_os("HOME").map(|h| {
                use std::os::unix::ffi::OsStrExt;
                h.as_bytes().to_vec()
            }),
        }
    }
}

/// What an indexing run did, with measured timings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexReport {
    pub record: Option<RecordSummary>,
    pub fingerprints: FingerprintReport,
    pub references: ReferenceReport,
    pub relations: RelationReport,
    pub edges: EdgeApplyReport,
    pub layout: LayoutReport,
    pub timings_ms: BTreeMap<String, u64>,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Canonicalises a user-supplied root (absolute, symlinks resolved) and builds a scan config.
pub fn scan_config_for(path: &Path) -> std::io::Result<ScanConfig> {
    let canonical = std::fs::canonicalize(path)?;
    Ok(ScanConfig::new(canonical))
}

/// Runs a scan synchronously with a progress callback.
pub fn run_scan(
    config: &ScanConfig,
    cancel: &CancelToken,
    on_progress: impl FnMut(&ScanProgress),
) -> ScanResult {
    scan_directory_with_progress(config, cancel, on_progress)
}

/// Records `scan` as a snapshot and refreshes relations and layout.
pub fn index_scan(
    store: &OrbitStore,
    scan: &ScanResult,
    label: Option<&str>,
    opts: &IndexOptions,
) -> Result<IndexReport, DbError> {
    let t = Instant::now();
    let record = store.record_scan(scan, label)?;
    let record_ms = t.elapsed().as_millis() as u64;
    let mut report = refresh(store, opts)?;
    report
        .timings_ms
        .insert("record_snapshot".into(), record_ms);
    report.record = Some(record);
    Ok(report)
}

/// Refreshes fingerprints, references, automatic relations and layout from the current store
/// state (no scan).
pub fn refresh(store: &OrbitStore, opts: &IndexOptions) -> Result<IndexReport, DbError> {
    let mut report = IndexReport::default();
    let mut nodes = store.load_nodes(true)?;
    nodes.retain(|n| n.state != EntityState::ProbablyDeleted);
    // Every stage filters `Present` entities itself; unobserved ones only take part in the layout.
    let present = nodes.as_slice();

    if !opts.skip_relations {
        let t = Instant::now();
        let cache = store.load_fingerprints()?;
        let (fps, fp_report) = refresh_fingerprints(present, &cache, &opts.fingerprint);
        let changed: Vec<_> = fps
            .iter()
            .filter(|f| cache.get(&f.entity_id) != Some(f))
            .cloned()
            .collect();
        store.save_fingerprints(&changed)?;
        report.fingerprints = fp_report;
        report
            .timings_ms
            .insert("fingerprints".into(), t.elapsed().as_millis() as u64);

        let t = Instant::now();
        let ref_cache = store.load_reference_cache()?;
        let (updated, current, ref_report) =
            refresh_references(present, &ref_cache, opts.home.as_deref());
        store.save_reference_cache(&updated)?;
        report.references = ref_report;
        report
            .timings_ms
            .insert("references".into(), t.elapsed().as_millis() as u64);

        let t = Instant::now();
        let co = store.load_co_activity()?;
        let inputs = RelationInputs {
            nodes: present,
            fingerprints: &fps,
            references: &current,
            co_activity: &co,
            now_unix_secs: now_secs(),
        };
        let (edges, rel_report) = compute_auto_edges(&inputs, &opts.relations);
        report.relations = rel_report;
        report
            .timings_ms
            .insert("relations".into(), t.elapsed().as_millis() as u64);

        let t = Instant::now();
        report.edges = store.apply_auto_edges(&edges)?;
        report
            .timings_ms
            .insert("edges_persist".into(), t.elapsed().as_millis() as u64);
    }

    let t = Instant::now();
    let canvas = store.canvas_id(MAIN_CANVAS)?;
    let existing = store.load_positions(canvas)?;
    let cfg = LayoutConfig {
        seed: store.canvas_seed(canvas)?,
        ..LayoutConfig::default()
    };
    let layout = update_layout(&nodes, &existing, &cfg);
    store.save_auto_positions(canvas, &layout.updates)?;
    report.layout = layout.report;
    report
        .timings_ms
        .insert("layout".into(), t.elapsed().as_millis() as u64);
    Ok(report)
}

/// Explicit full re-layout of the main canvas (user request). Pinned and manual positions are
/// preserved; `seed` replaces the canvas seed when given.
pub fn relayout(store: &OrbitStore, seed: Option<u64>) -> Result<LayoutReport, DbError> {
    let canvas = store.canvas_id(MAIN_CANVAS)?;
    if let Some(s) = seed {
        store.set_canvas_seed(canvas, s)?;
    }
    let mut nodes = store.load_nodes(true)?;
    nodes.retain(|n| n.state != EntityState::ProbablyDeleted);
    let existing = store.load_positions(canvas)?;
    let prev_version = existing
        .values()
        .map(|p| p.layout_version)
        .max()
        .unwrap_or(0);
    let cfg = LayoutConfig {
        seed: store.canvas_seed(canvas)?,
        layout_version: prev_version + 1,
        ..LayoutConfig::default()
    };
    let result = crate::layout::relayout_all(&nodes, &existing, &cfg);
    store.save_auto_positions(canvas, &result.updates)?;
    Ok(result.report)
}
