//! Authoritative domain types.
//!
//! The first part (scan arena: [`EntryNode`], [`ScanResult`], ...) is inherited from DiskGarden and
//! describes a single scan. The second part ([`EntityState`], [`GraphNode`], [`Edge`], ...) is the
//! ORBIT layer: persistent identities, relations and space.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Unique identifier of an entry inside [`ScanResult::entries`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EntryId(pub usize);

/// Filesystem object type detected via `lstat` (`symlink_metadata`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryType {
    Directory,
    File,
    Symlink,
    Other,
}

/// Access and validity status of a filesystem entry during scanning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessStatus {
    /// Entry metadata (and directory listing, if a directory) was read completely.
    Ok,
    /// Access was denied by the OS (`EACCES` or `EPERM`). Never treated as an empty directory.
    PermissionDenied,
    /// A generic I/O error occurred while reading metadata or directory entries.
    IoError,
    /// The entry disappeared between directory enumeration and `lstat` (`ENOENT`).
    Vanished,
    /// The directory resides on a different mount point / device (`st_dev != root_dev`)
    /// and `cross_mounts` was `false`.
    ExcludedMount,
    /// The entry is a symbolic link and `follow_symlinks` was `false`.
    SymlinkSkipped,
    /// Following a symbolic link or bind mount would have caused a directory cycle.
    SymlinkCycleSkipped,
}

impl AccessStatus {
    /// Returns `true` if this status represents an access or I/O error that makes the scan partial.
    pub fn is_error(self) -> bool {
        matches!(self, AccessStatus::PermissionDenied | AccessStatus::IoError)
    }
}

/// Overall lifecycle status of a scan operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanStatus {
    /// Entire hierarchy was traversed without access or I/O errors.
    Completed,
    /// Traversal finished, but one or more entries could not be read (`PermissionDenied` or `IoError`).
    Partial,
    /// Traversal was explicitly cancelled via [`crate::scanner::CancelToken`].
    Cancelled,
    /// The root path itself did not exist or could not be opened at the start of the scan.
    Failed,
}

/// Which size metric to use when sorting, filtering, or computing visual layouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SizeMetric {
    /// Physical space allocated by the filesystem (`st_blocks * 512`).
    Allocated,
    /// Logical byte length reported by `st_size`.
    Apparent,
}

/// Configuration parameters for a directory scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanConfig {
    /// Root directory to scan.
    pub root_path: PathBuf,
    /// Whether to follow symbolic links. Defaults to `false` for safety.
    pub follow_symlinks: bool,
    /// Whether to cross into different filesystem mount points (`st_dev != root_dev`). Defaults to `false`.
    pub cross_mounts: bool,
    /// Minimum interval in milliseconds between emitted progress updates.
    pub progress_interval_ms: u64,
    /// Absolute directory paths that are never traversed (recorded as `ExcludedMount` with an
    /// explicit message, never silently hidden). Defaults to the Linux kernel pseudo-filesystems
    /// `/proc`, `/sys`, `/dev` and `/run` (see [`ScanConfig::default_excluded_paths`]).
    #[serde(default = "ScanConfig::default_excluded_paths")]
    pub excluded_paths: Vec<PathBuf>,
    /// Directory names that are never traversed wherever they appear: dependency, VCS and build
    /// directories that would drown a project's own files (recorded like `excluded_paths`).
    /// `target` is only excluded next to a `Cargo.toml`, and while this list is non-empty any
    /// directory holding a `pyvenv.cfg` (a Python virtualenv) is excluded too. See
    /// [`ScanConfig::default_excluded_names`].
    #[serde(default = "ScanConfig::default_excluded_names")]
    pub excluded_names: Vec<String>,
}

impl ScanConfig {
    /// Creates a safe default scan configuration for `root_path`.
    pub fn new(root_path: impl Into<PathBuf>) -> Self {
        Self {
            root_path: root_path.into(),
            follow_symlinks: false,
            cross_mounts: false,
            progress_interval_ms: 75,
            excluded_paths: Self::default_excluded_paths(),
            excluded_names: Self::default_excluded_names(),
        }
    }

    /// Default exclusions: Linux kernel pseudo-filesystems that must not be walked when the
    /// selected root contains them (e.g. scanning `/`).
    pub fn default_excluded_paths() -> Vec<PathBuf> {
        ["/proc", "/sys", "/dev", "/run"]
            .iter()
            .map(PathBuf::from)
            .collect()
    }

    /// Default name exclusions: version control, dependency, virtualenv, tool cache and build
    /// output directories.
    pub fn default_excluded_names() -> Vec<String> {
        [
            ".git",
            ".hg",
            ".svn",
            "node_modules",
            ".venv",
            "venv",
            "__pycache__",
            ".mypy_cache",
            ".pytest_cache",
            ".ruff_cache",
            ".tox",
            ".gradle",
            ".next",
            ".nuxt",
            ".flatpak-builder",
            "target",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }
}

/// Real-time non-blocking progress snapshot emitted during an active scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanProgress {
    pub dirs_scanned: u64,
    pub files_scanned: u64,
    pub symlinks_scanned: u64,
    pub apparent_bytes_so_far: u64,
    pub allocated_bytes_so_far: u64,
    pub errors_count: u64,
    pub current_rel_path: String,
    pub elapsed_ms: u64,
}

/// A single node (directory, file, symlink, or special file) in the scanned tree arena.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryNode {
    pub id: EntryId,
    pub parent_id: Option<EntryId>,
    pub children_ids: Vec<EntryId>,
    /// Raw POSIX bytes of the path relative to the scan root (empty `b""` for root).
    pub rel_path_raw: Vec<u8>,
    /// Lossy UTF-8 string representation of the relative path for display and search.
    pub rel_path_display: String,
    /// Raw POSIX bytes of the entry's base name.
    pub name_raw: Vec<u8>,
    /// Lossy UTF-8 string representation of the entry's base name.
    pub name_display: String,
    /// `true` if `rel_path_raw` is valid UTF-8, `false` if it contains non-UTF-8 bytes.
    pub is_valid_utf8: bool,
    /// Depth relative to root (`0` for the root directory).
    pub depth: u32,
    pub entry_type: EntryType,
    /// Own logical byte size (`st_size`). For directories, this is the directory inode metadata size.
    pub apparent_bytes: u64,
    /// Own physical allocated byte size (`st_blocks * 512`).
    pub allocated_bytes: u64,
    /// Total logical bytes of this node and all its descendants (deduplicated for hard links).
    pub subtree_apparent_bytes: u64,
    /// Total physical allocated bytes of this node and all its descendants (deduplicated for hard links).
    pub subtree_allocated_bytes: u64,
    /// Sum of `apparent_bytes` of direct child files in this directory.
    pub direct_files_apparent_bytes: u64,
    /// Sum of `allocated_bytes` of direct child files in this directory.
    pub direct_files_allocated_bytes: u64,
    /// Number of regular files in this node's subtree.
    pub subtree_files_count: u64,
    /// Number of subdirectories in this node's subtree (excluding self).
    pub subtree_dirs_count: u64,
    /// Number of symbolic links in this node's subtree.
    pub subtree_symlinks_count: u64,
    /// Number of access/IO errors encountered in this node or its descendants.
    pub subtree_error_count: u64,
    /// POSIX device ID (`st_dev`).
    pub dev: u64,
    /// POSIX inode number (`st_ino`).
    pub ino: u64,
    /// POSIX hard link count (`st_nlink`).
    pub nlink: u64,
    /// `true` if another entry with the same `(dev, ino)` was already counted earlier in this scan.
    pub is_hardlink_duplicate: bool,
    /// If `is_hardlink_duplicate` is `true`, points to the first [`EntryId`] that counted this inode.
    pub primary_hardlink_id: Option<EntryId>,
    /// Total number of paths in this scan that share this `(dev, ino)`.
    pub shared_link_paths_in_scan: u32,
    /// Last modification time (seconds since Unix epoch).
    pub mtime_sec: Option<i64>,
    /// Last modification time nanoseconds part.
    pub mtime_nsec: Option<u32>,
    pub access_status: AccessStatus,
    pub error_message: Option<String>,
}

impl EntryNode {
    /// Returns the effective size of this entry (subtree size for directories, own size for files)
    /// according to the chosen [`SizeMetric`].
    pub fn effective_bytes(&self, metric: SizeMetric) -> u64 {
        match (self.entry_type, metric) {
            (EntryType::Directory, SizeMetric::Allocated) => self.subtree_allocated_bytes,
            (EntryType::Directory, SizeMetric::Apparent) => self.subtree_apparent_bytes,
            (_, SizeMetric::Allocated) => self.allocated_bytes,
            (_, SizeMetric::Apparent) => self.apparent_bytes,
        }
    }

    /// Returns `true` if this is a regular file whose physical allocated space is smaller than
    /// its logical apparent size (sparse file).
    pub fn is_sparse(&self) -> bool {
        self.entry_type == EntryType::File
            && self.apparent_bytes > 0
            && self.allocated_bytes < self.apparent_bytes
    }

    /// Returns `true` if this non-directory entry has hard links (`st_nlink > 1`) that are not
    /// all accounted for inside the scanned root directory.
    pub fn has_external_hardlinks(&self) -> bool {
        self.entry_type != EntryType::Directory
            && self.nlink > 1
            && (self.shared_link_paths_in_scan as u64) < self.nlink
    }
}

/// Complete, authoritative result of a directory scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub root_path_raw: Vec<u8>,
    pub root_path_display: String,
    pub root_dev: u64,
    pub config: ScanConfig,
    pub status: ScanStatus,
    pub started_at_unix_ms: i64,
    pub duration_ms: u64,
    pub root_id: Option<EntryId>,
    pub entries: Vec<EntryNode>,
    /// Total logical bytes across the scanned root (deduplicated for hard links).
    pub total_apparent_bytes: u64,
    /// Total physical allocated bytes across the scanned root (deduplicated for hard links).
    pub total_allocated_bytes: u64,
    pub total_files_count: u64,
    /// Total subdirectories inside the scanned root (excluding the root directory itself).
    pub total_dirs_count: u64,
    pub total_symlinks_count: u64,
    /// Number of additional hard-link paths detected that share an already-counted inode.
    pub hardlink_duplicate_files_count: u64,
    /// Physical allocated bytes saved from double-counting due to hard-link deduplication.
    pub hardlink_saved_allocated_bytes: u64,
    /// Apparent bytes saved from double-counting due to hard-link deduplication.
    pub hardlink_saved_apparent_bytes: u64,
    pub unreadable_dirs_count: u64,
    pub permission_denied_count: u64,
    pub io_errors_count: u64,
    pub vanished_count: u64,
    pub excluded_mounts_count: u64,
    pub symlink_cycles_count: u64,
    pub root_error_message: Option<String>,
}

impl ScanResult {
    /// Looks up an [`EntryNode`] by its [`EntryId`].
    pub fn get(&self, id: EntryId) -> Option<&EntryNode> {
        self.entries.get(id.0)
    }

    /// Returns the root [`EntryNode`] if the scan created one.
    pub fn root(&self) -> Option<&EntryNode> {
        self.root_id.and_then(|id| self.get(id))
    }
}

// ================================================================================================
// ORBIT layer: persistent identity, relations, space
// ================================================================================================

/// Database identifier of a persistent [`crate::store::OrbitStore`] file entity.
pub type EntityId = i64;

/// Lifecycle of a persistent file entity across snapshots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityState {
    /// Seen by the latest scan that covered its location.
    Present,
    /// Not seen, but the covering scan could not prove absence (partial or cancelled scan,
    /// unreadable or excluded ancestor).
    Unobserved,
    /// Not seen by a complete scan whose nearest observed ancestor was read successfully.
    /// ORBIT never claims a file is deleted with certainty.
    ProbablyDeleted,
}

impl EntityState {
    pub fn as_db(self) -> &'static str {
        match self {
            EntityState::Present => "present",
            EntityState::Unobserved => "unobserved",
            EntityState::ProbablyDeleted => "probably_deleted",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "present" => EntityState::Present,
            "probably_deleted" => EntityState::ProbablyDeleted,
            _ => EntityState::Unobserved,
        }
    }
}

/// How an observation was linked to a persistent entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityMatch {
    /// No sufficient evidence: a new entity was created.
    New,
    /// Same path and same inode.
    SamePath,
    /// Same path but a different inode (atomic save, replacement).
    SamePathReplaced,
    /// Different path, same inode and unchanged size/mtime.
    Moved,
    /// Different path, same inode and same name, but content metadata changed.
    MovedModified,
    /// Different path and inode (e.g. cross-filesystem move) with identical name, size and mtime.
    MovedByContentMetadata,
}

impl IdentityMatch {
    pub fn as_db(self) -> &'static str {
        match self {
            IdentityMatch::New => "new",
            IdentityMatch::SamePath => "same_path",
            IdentityMatch::SamePathReplaced => "same_path_replaced",
            IdentityMatch::Moved => "moved",
            IdentityMatch::MovedModified => "moved_modified",
            IdentityMatch::MovedByContentMetadata => "moved_by_content_metadata",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "same_path" => IdentityMatch::SamePath,
            "same_path_replaced" => IdentityMatch::SamePathReplaced,
            "moved" => IdentityMatch::Moved,
            "moved_modified" => IdentityMatch::MovedModified,
            "moved_by_content_metadata" => IdentityMatch::MovedByContentMetadata,
            _ => IdentityMatch::New,
        }
    }

    /// `true` when the entity changed location.
    pub fn is_move(self) -> bool {
        matches!(
            self,
            IdentityMatch::Moved
                | IdentityMatch::MovedModified
                | IdentityMatch::MovedByContentMetadata
        )
    }
}

/// Relation kinds. `ParentChild` mirrors the physical hierarchy; all others are overlays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    ParentChild,
    TemporalAffinity,
    NameSimilarity,
    FingerprintMatch,
    VerifiedDuplicate,
    ExplicitReference,
    ManualAssociation,
    ProjectMembership,
}

impl RelationKind {
    pub const ALL: [RelationKind; 8] = [
        RelationKind::ParentChild,
        RelationKind::TemporalAffinity,
        RelationKind::NameSimilarity,
        RelationKind::FingerprintMatch,
        RelationKind::VerifiedDuplicate,
        RelationKind::ExplicitReference,
        RelationKind::ManualAssociation,
        RelationKind::ProjectMembership,
    ];

    pub fn as_db(self) -> &'static str {
        match self {
            RelationKind::ParentChild => "parent_child",
            RelationKind::TemporalAffinity => "temporal_affinity",
            RelationKind::NameSimilarity => "name_similarity",
            RelationKind::FingerprintMatch => "fingerprint_match",
            RelationKind::VerifiedDuplicate => "verified_duplicate",
            RelationKind::ExplicitReference => "explicit_reference",
            RelationKind::ManualAssociation => "manual_association",
            RelationKind::ProjectMembership => "project_membership",
        }
    }

    pub fn from_db(s: &str) -> Option<Self> {
        RelationKind::ALL.into_iter().find(|k| k.as_db() == s)
    }

    /// `true` for relations whose direction carries meaning (`a` references `b`).
    pub fn is_directed(self) -> bool {
        matches!(
            self,
            RelationKind::ParentChild | RelationKind::ExplicitReference
        )
    }
}

/// Who owns an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeOrigin {
    /// Produced by the relation engine; may be refreshed or marked stale on recomputation.
    Auto,
    /// Created by the user; never modified by recomputation.
    Manual,
    /// Derived on the fly from the filesystem hierarchy (ParentChild); never stored.
    Filesystem,
}

impl EdgeOrigin {
    pub fn as_db(self) -> &'static str {
        match self {
            EdgeOrigin::Auto => "auto",
            EdgeOrigin::Manual => "manual",
            EdgeOrigin::Filesystem => "filesystem",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "manual" => EdgeOrigin::Manual,
            "filesystem" => EdgeOrigin::Filesystem,
            _ => EdgeOrigin::Auto,
        }
    }
}

/// Validity of a stored edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeState {
    Active,
    /// Removed from the view by the user; recomputation keeps it hidden.
    Hidden,
    /// No longer produced by the engine, or an endpoint is no longer present.
    Stale,
    /// Manual edge soft-deleted by the user (restorable).
    Deleted,
}

impl EdgeState {
    pub fn as_db(self) -> &'static str {
        match self {
            EdgeState::Active => "active",
            EdgeState::Hidden => "hidden",
            EdgeState::Stale => "stale",
            EdgeState::Deleted => "deleted",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "hidden" => EdgeState::Hidden,
            "stale" => EdgeState::Stale,
            "deleted" => EdgeState::Deleted,
            _ => EdgeState::Active,
        }
    }
}

/// A relation between two persistent entities, with weight, confidence and explanation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    /// Database id (`None` for derived ParentChild edges or edges not yet stored).
    pub id: Option<i64>,
    pub src: EntityId,
    pub dst: EntityId,
    pub kind: RelationKind,
    pub origin: EdgeOrigin,
    /// Normalised heuristic strength in `[0, 1]`. Not a probability.
    pub weight: f64,
    /// How much the engine trusts the evidence, in `[0, 1]`.
    pub confidence: f64,
    /// Machine-readable source of the evidence (e.g. `name:root=contratto`, `markdown:line=12`).
    pub provenance: String,
    /// Human-readable explanations shown in the UI.
    pub reasons: Vec<String>,
    pub state: EdgeState,
}

impl Edge {
    /// Combined strength used for ranking: `weight * confidence`.
    pub fn strength(&self) -> f64 {
        self.weight * self.confidence
    }
}

/// The current state of a persistent entity, as used by the relation, layout and search engines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GraphNode {
    pub entity_id: EntityId,
    pub kind: EntryType,
    pub state: EntityState,
    #[serde(skip)]
    pub path_raw: Vec<u8>,
    pub path_display: String,
    pub name: String,
    pub parent_entity_id: Option<EntityId>,
    pub depth: u32,
    pub apparent_bytes: u64,
    pub allocated_bytes: u64,
    pub subtree_apparent_bytes: u64,
    pub subtree_files_count: u64,
    pub mtime_sec: Option<i64>,
    pub mtime_nsec: Option<u32>,
    pub dev: Option<u64>,
    pub ino: Option<u64>,
    pub nlink: u64,
    pub is_hardlink_duplicate: bool,
    pub access_status: AccessStatus,
}

impl GraphNode {
    /// Size used for display: subtree size for directories, own apparent size otherwise.
    pub fn display_bytes(&self) -> u64 {
        if self.kind == EntryType::Directory {
            self.subtree_apparent_bytes
        } else {
            self.apparent_bytes
        }
    }

    /// Lower-case extension without the dot (`""` if none).
    pub fn extension(&self) -> String {
        extension_of(&self.name)
    }
}

/// Lower-case extension of a file name without the dot (`""` if none or if the name is a dotfile).
pub fn extension_of(name: &str) -> String {
    let trimmed = name.trim_start_matches('.');
    match trimmed.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() && ext.len() <= 16 => {
            ext.to_lowercase()
        }
        _ => String::new(),
    }
}

impl EntryType {
    pub fn as_db(self) -> &'static str {
        match self {
            EntryType::Directory => "dir",
            EntryType::File => "file",
            EntryType::Symlink => "symlink",
            EntryType::Other => "other",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "dir" => EntryType::Directory,
            "file" => EntryType::File,
            "symlink" => EntryType::Symlink,
            _ => EntryType::Other,
        }
    }
}

impl AccessStatus {
    pub fn as_db(self) -> &'static str {
        match self {
            AccessStatus::Ok => "ok",
            AccessStatus::PermissionDenied => "permission_denied",
            AccessStatus::IoError => "io_error",
            AccessStatus::Vanished => "vanished",
            AccessStatus::ExcludedMount => "excluded_mount",
            AccessStatus::SymlinkSkipped => "symlink_skipped",
            AccessStatus::SymlinkCycleSkipped => "symlink_cycle_skipped",
        }
    }

    pub fn from_db(s: &str) -> Self {
        match s {
            "ok" => AccessStatus::Ok,
            "permission_denied" => AccessStatus::PermissionDenied,
            "vanished" => AccessStatus::Vanished,
            "excluded_mount" => AccessStatus::ExcludedMount,
            "symlink_skipped" => AccessStatus::SymlinkSkipped,
            "symlink_cycle_skipped" => AccessStatus::SymlinkCycleSkipped,
            _ => AccessStatus::IoError,
        }
    }
}

impl ScanStatus {
    pub fn as_db(self) -> &'static str {
        match self {
            ScanStatus::Completed => "completed",
            ScanStatus::Partial => "partial",
            ScanStatus::Cancelled => "cancelled",
            ScanStatus::Failed => "failed",
        }
    }
}
