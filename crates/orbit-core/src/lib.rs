//! # `orbit-core`
//!
//! UI-independent core of **ORBIT**, a spatial and relational explorer of the filesystem.
//!
//! - [`scanner`], [`model`]: robust Linux filesystem scanner and scan arena (inherited from DiskGarden).
//! - [`identity`]: best-effort persistent identity across scans, with explicit uncertainty.
//! - [`store`]: SQLite persistence (snapshots, entities, edges, positions, constellations).
//! - [`fingerprint`], [`references`], [`relations`]: sparse, explainable relation engine.
//! - [`layout`]: deterministic, persistent, incremental spatial layout.
//! - [`graph`]: in-memory read model and level-of-detail views (galaxy, system, file).
//! - [`search`]: query parsing and ranking for the gravitational search.
//! - [`history`]: entity-based snapshot comparison.
//! - [`analyzer`]: scan helpers and the informative (non-destructive) space report.
//! - [`index`]: the pipeline that ties the stages together.

pub mod analyzer;
pub mod fingerprint;
pub mod graph;
pub mod history;
pub mod identity;
pub mod index;
pub mod layout;
pub mod model;
pub mod references;
pub mod relations;
pub mod scanner;
pub mod search;
mod sqlite;
pub mod store;

pub use model::{
    AccessStatus, Edge, EdgeOrigin, EdgeState, EntityId, EntityState, EntryId, EntryNode,
    EntryType, GraphNode, IdentityMatch, RelationKind, ScanConfig, ScanProgress, ScanResult,
    ScanStatus, SizeMetric,
};
pub use scanner::{
    list_system_volumes, raw_bytes_to_path_buf, scan_directory, scan_directory_with_progress,
    CancelToken, DiskVolumeInfo, ScanEvent, ScanHandle,
};
pub use store::{DbError, OrbitStore};
