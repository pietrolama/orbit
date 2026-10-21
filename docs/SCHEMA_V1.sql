-- ORBIT — SQLite schema v1
--
-- This file is the authoritative definition of schema version 1. It is embedded into the binary
-- (`orbit-core/src/store.rs`, `include_str!`) and applied by the migration runner when
-- `PRAGMA user_version` is 0. Every statement is idempotent (`IF NOT EXISTS`).
--
-- Migration strategy
--   * `PRAGMA user_version` holds the applied schema version; `schema_migrations` keeps an audit
--     log of when each version was applied.
--   * Future versions are applied as ordered, append-only migration scripts (v2, v3, ...) inside
--     one transaction each. A database whose version is newer than the binary supports is refused
--     (never downgraded or rewritten).
--
-- Conceptual layers (see docs/ARCHITECTURE.md):
--   observations  : snapshots, snapshot_entries         (what a scan saw, immutable history)
--   identity      : file_entities, identity_candidates   (best-effort persistent identity)
--   content       : fingerprints, extracted_references   (cached evidence tied to a file version)
--   relations     : edges, co_activity                   (sparse, explainable graph)
--   space         : spatial_entities, canvases, node_positions, constellations, constellation_members
--   preferences   : user_preferences
--
-- Paths are stored as raw POSIX bytes (BLOB, absolute) plus a lossy UTF-8 display copy, so
-- non-UTF-8 names survive round trips.

PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY NOT NULL,
    applied_at_unix_ms INTEGER NOT NULL
);

-- ---------------------------------------------------------------------------------------------
-- Observations
-- ---------------------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    root_path_raw BLOB NOT NULL,
    root_path_display TEXT NOT NULL,
    label TEXT,
    created_at_unix_ms INTEGER NOT NULL,
    scan_duration_ms INTEGER NOT NULL DEFAULT 0,
    -- 'saving' is transient: a crash leaves it behind and the next open marks it 'interrupted'.
    status TEXT NOT NULL CHECK (status IN ('saving', 'completed', 'partial', 'cancelled', 'interrupted')),
    root_dev INTEGER NOT NULL DEFAULT 0,
    cross_mounts INTEGER NOT NULL DEFAULT 0,
    follow_symlinks INTEGER NOT NULL DEFAULT 0,
    total_apparent_bytes INTEGER NOT NULL DEFAULT 0,
    total_allocated_bytes INTEGER NOT NULL DEFAULT 0,
    total_files_count INTEGER NOT NULL DEFAULT 0,
    total_dirs_count INTEGER NOT NULL DEFAULT 0,
    total_symlinks_count INTEGER NOT NULL DEFAULT 0,
    permission_denied_count INTEGER NOT NULL DEFAULT 0,
    io_errors_count INTEGER NOT NULL DEFAULT 0,
    vanished_count INTEGER NOT NULL DEFAULT 0,
    excluded_count INTEGER NOT NULL DEFAULT 0,
    -- Identity resolution summary for this snapshot.
    new_entities_count INTEGER NOT NULL DEFAULT 0,
    matched_entities_count INTEGER NOT NULL DEFAULT 0,
    moved_entities_count INTEGER NOT NULL DEFAULT 0,
    ambiguous_entries_count INTEGER NOT NULL DEFAULT 0,
    unobserved_entities_count INTEGER NOT NULL DEFAULT 0,
    probably_deleted_entities_count INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_snapshots_root_time
    ON snapshots(root_path_raw, created_at_unix_ms DESC);

-- Persistent identity (declared before snapshot_entries, which references it).
CREATE TABLE IF NOT EXISTS file_entities (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL CHECK (kind IN ('dir', 'file', 'symlink', 'other')),
    -- 'present'          : seen in the latest snapshot that covered its location
    -- 'unobserved'       : not seen, but the covering scan could not prove absence
    --                      (partial/cancelled scan, unreadable ancestor, outside the scanned root)
    -- 'probably_deleted' : not seen by a complete scan whose parent directory was read successfully.
    --                      Never "deleted with certainty".
    state TEXT NOT NULL CHECK (state IN ('present', 'unobserved', 'probably_deleted')),
    first_seen_snapshot_id INTEGER NOT NULL,
    last_seen_snapshot_id INTEGER NOT NULL,
    last_path_raw BLOB NOT NULL,
    last_path_display TEXT NOT NULL,
    last_name_display TEXT NOT NULL,
    parent_entity_id INTEGER REFERENCES file_entities(id) ON DELETE SET NULL,
    depth INTEGER NOT NULL DEFAULT 0,
    last_dev INTEGER,
    last_ino INTEGER,
    last_apparent_bytes INTEGER NOT NULL DEFAULT 0,
    last_allocated_bytes INTEGER NOT NULL DEFAULT 0,
    subtree_apparent_bytes INTEGER NOT NULL DEFAULT 0,
    subtree_files_count INTEGER NOT NULL DEFAULT 0,
    last_mtime_sec INTEGER,
    last_mtime_nsec INTEGER,
    nlink INTEGER NOT NULL DEFAULT 1,
    is_hardlink_duplicate INTEGER NOT NULL DEFAULT 0,
    access_status TEXT NOT NULL DEFAULT 'ok',
    created_at_unix_ms INTEGER NOT NULL,
    state_changed_at_unix_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_entities_path ON file_entities(last_path_raw);
CREATE INDEX IF NOT EXISTS idx_entities_state_kind ON file_entities(state, kind);
CREATE INDEX IF NOT EXISTS idx_entities_parent ON file_entities(parent_entity_id);
CREATE INDEX IF NOT EXISTS idx_entities_inode ON file_entities(last_dev, last_ino);
CREATE INDEX IF NOT EXISTS idx_entities_mtime ON file_entities(last_mtime_sec);

CREATE TABLE IF NOT EXISTS snapshot_entries (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
    entity_id INTEGER REFERENCES file_entities(id) ON DELETE SET NULL,
    path_raw BLOB NOT NULL,
    path_display TEXT NOT NULL,
    name_display TEXT NOT NULL,
    is_valid_utf8 INTEGER NOT NULL DEFAULT 1,
    depth INTEGER NOT NULL,
    entry_type TEXT NOT NULL CHECK (entry_type IN ('dir', 'file', 'symlink', 'other')),
    apparent_bytes INTEGER NOT NULL DEFAULT 0,
    allocated_bytes INTEGER NOT NULL DEFAULT 0,
    subtree_apparent_bytes INTEGER NOT NULL DEFAULT 0,
    subtree_allocated_bytes INTEGER NOT NULL DEFAULT 0,
    subtree_files_count INTEGER NOT NULL DEFAULT 0,
    dev INTEGER,
    ino INTEGER,
    nlink INTEGER NOT NULL DEFAULT 1,
    is_hardlink_duplicate INTEGER NOT NULL DEFAULT 0,
    mtime_sec INTEGER,
    mtime_nsec INTEGER,
    access_status TEXT NOT NULL CHECK (
        access_status IN ('ok', 'permission_denied', 'io_error', 'vanished', 'excluded_mount',
                          'symlink_skipped', 'symlink_cycle_skipped')
    ),
    error_message TEXT,
    -- How this observation was linked to its entity:
    -- 'new' | 'same_path' | 'same_path_replaced' | 'moved' | 'moved_modified' | 'moved_by_content_metadata'
    identity_match TEXT NOT NULL DEFAULT 'new',
    identity_confidence REAL NOT NULL DEFAULT 1.0,
    UNIQUE(snapshot_id, path_raw)
);

CREATE INDEX IF NOT EXISTS idx_entries_entity ON snapshot_entries(entity_id, snapshot_id);
CREATE INDEX IF NOT EXISTS idx_entries_snapshot_type ON snapshot_entries(snapshot_id, entry_type);

-- Matches that were plausible but not certain enough to merge identities. Kept so the UI/CLI can
-- show "this might be the same file as ..." instead of silently inventing a link.
CREATE TABLE IF NOT EXISTS identity_candidates (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    snapshot_id INTEGER NOT NULL REFERENCES snapshots(id) ON DELETE CASCADE,
    entity_id INTEGER NOT NULL REFERENCES file_entities(id) ON DELETE CASCADE,
    candidate_entity_id INTEGER NOT NULL REFERENCES file_entities(id) ON DELETE CASCADE,
    confidence REAL NOT NULL,
    reason TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_identity_candidates_entity ON identity_candidates(entity_id);

-- ---------------------------------------------------------------------------------------------
-- Content evidence
-- ---------------------------------------------------------------------------------------------

-- A fingerprint is valid only for the exact observed version (size, mtime, dev, ino). Any change
-- in those fields invalidates it (the row is replaced on the next computation).
CREATE TABLE IF NOT EXISTS fingerprints (
    entity_id INTEGER PRIMARY KEY NOT NULL REFERENCES file_entities(id) ON DELETE CASCADE,
    size_bytes INTEGER NOT NULL,
    mtime_sec INTEGER,
    mtime_nsec INTEGER,
    dev INTEGER,
    ino INTEGER,
    -- BLAKE3 of (size || first 64 KiB || last 64 KiB). For files <= 128 KiB it covers the full content.
    sample_hash TEXT NOT NULL,
    sample_covers_full_file INTEGER NOT NULL DEFAULT 0,
    -- BLAKE3 of the full content, computed only on explicit verification.
    full_hash TEXT,
    computed_at_unix_ms INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_fingerprints_sample ON fingerprints(size_bytes, sample_hash);

-- Local references extracted from Markdown / plain-text files, cached per source version.
CREATE TABLE IF NOT EXISTS extracted_references (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_entity_id INTEGER NOT NULL REFERENCES file_entities(id) ON DELETE CASCADE,
    source_size_bytes INTEGER NOT NULL,
    source_mtime_sec INTEGER,
    source_mtime_nsec INTEGER,
    -- Absolute, lexically normalised target path; NULL row target means "parsed, no references".
    target_path_raw BLOB,
    line INTEGER NOT NULL DEFAULT 0,
    syntax TEXT NOT NULL DEFAULT 'none'
);

CREATE INDEX IF NOT EXISTS idx_refs_source ON extracted_references(source_entity_id);

-- ---------------------------------------------------------------------------------------------
-- Relations
-- ---------------------------------------------------------------------------------------------

-- ParentChild is not stored here: it is derived from file_entities.parent_entity_id (the
-- filesystem is the source of truth). All other relation kinds live in this table.
CREATE TABLE IF NOT EXISTS edges (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    src_entity_id INTEGER NOT NULL REFERENCES file_entities(id) ON DELETE CASCADE,
    dst_entity_id INTEGER NOT NULL REFERENCES file_entities(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('temporal_affinity', 'name_similarity', 'fingerprint_match',
                                       'verified_duplicate', 'explicit_reference',
                                       'manual_association', 'project_membership')),
    -- 'auto' edges are owned by the relation engine; 'manual' edges are owned by the user and are
    -- never modified or removed by recomputation.
    origin TEXT NOT NULL CHECK (origin IN ('auto', 'manual')),
    weight REAL NOT NULL CHECK (weight >= 0.0 AND weight <= 1.0),
    confidence REAL NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
    provenance TEXT NOT NULL,
    -- JSON array of human-readable reasons.
    reasons_json TEXT NOT NULL DEFAULT '[]',
    -- 'active'  : valid and shown
    -- 'hidden'  : user removed it from the view; recomputation keeps it hidden
    -- 'stale'   : no longer produced by the engine (or an endpoint is not present); kept for history
    -- 'deleted' : manual edge deleted by the user (soft delete, restorable)
    state TEXT NOT NULL CHECK (state IN ('active', 'hidden', 'stale', 'deleted')),
    created_at_unix_ms INTEGER NOT NULL,
    updated_at_unix_ms INTEGER NOT NULL,
    CHECK (src_entity_id <> dst_entity_id),
    UNIQUE(src_entity_id, dst_entity_id, kind, origin)
);

CREATE INDEX IF NOT EXISTS idx_edges_src ON edges(src_entity_id, state);
CREATE INDEX IF NOT EXISTS idx_edges_dst ON edges(dst_entity_id, state);
CREATE INDEX IF NOT EXISTS idx_edges_kind ON edges(kind, state);

-- Aggregated co-activity: how often two entities changed in the same observation window across
-- snapshots. A derived, heuristic signal; never proof of project membership.
CREATE TABLE IF NOT EXISTS co_activity (
    entity_a INTEGER NOT NULL REFERENCES file_entities(id) ON DELETE CASCADE,
    entity_b INTEGER NOT NULL REFERENCES file_entities(id) ON DELETE CASCADE,
    co_changes INTEGER NOT NULL DEFAULT 0,
    last_snapshot_id INTEGER NOT NULL,
    updated_at_unix_ms INTEGER NOT NULL,
    CHECK (entity_a < entity_b),
    PRIMARY KEY (entity_a, entity_b)
);

-- ---------------------------------------------------------------------------------------------
-- Space
-- ---------------------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS canvases (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    layout_seed INTEGER NOT NULL DEFAULT 1,
    created_at_unix_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS constellations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    color TEXT,
    note TEXT,
    created_at_unix_ms INTEGER NOT NULL,
    updated_at_unix_ms INTEGER NOT NULL,
    -- Soft delete keeps the constellation restorable.
    deleted_at_unix_ms INTEGER
);

-- What the user recognises in space. Survives rescans and moves because it points to a
-- persistent file entity (or a constellation), never to a single observation.
CREATE TABLE IF NOT EXISTS spatial_entities (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_type TEXT NOT NULL CHECK (entity_type IN ('file_entity', 'constellation')),
    file_entity_id INTEGER UNIQUE REFERENCES file_entities(id) ON DELETE CASCADE,
    constellation_id INTEGER UNIQUE REFERENCES constellations(id) ON DELETE CASCADE,
    created_at_unix_ms INTEGER NOT NULL,
    CHECK ((entity_type = 'file_entity' AND file_entity_id IS NOT NULL AND constellation_id IS NULL)
        OR (entity_type = 'constellation' AND constellation_id IS NOT NULL AND file_entity_id IS NULL))
);

CREATE TABLE IF NOT EXISTS node_positions (
    spatial_entity_id INTEGER NOT NULL REFERENCES spatial_entities(id) ON DELETE CASCADE,
    canvas_id INTEGER NOT NULL REFERENCES canvases(id) ON DELETE CASCADE,
    x REAL NOT NULL,
    y REAL NOT NULL,
    radius REAL NOT NULL DEFAULT 1.0,
    -- pinned: automatic layout never moves this node.
    pinned INTEGER NOT NULL DEFAULT 0,
    -- 'auto' (layout engine) or 'manual' (user drag). Automatic layout never overwrites 'manual'.
    source TEXT NOT NULL DEFAULT 'auto' CHECK (source IN ('auto', 'manual')),
    cluster_id INTEGER,
    layout_version INTEGER NOT NULL DEFAULT 1,
    updated_at_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (spatial_entity_id, canvas_id)
);

CREATE INDEX IF NOT EXISTS idx_positions_canvas ON node_positions(canvas_id, x, y);

CREATE TABLE IF NOT EXISTS constellation_members (
    constellation_id INTEGER NOT NULL REFERENCES constellations(id) ON DELETE CASCADE,
    file_entity_id INTEGER NOT NULL REFERENCES file_entities(id) ON DELETE CASCADE,
    added_at_unix_ms INTEGER NOT NULL,
    PRIMARY KEY (constellation_id, file_entity_id)
);

-- ---------------------------------------------------------------------------------------------
-- Preferences
-- ---------------------------------------------------------------------------------------------

CREATE TABLE IF NOT EXISTS user_preferences (
    key TEXT PRIMARY KEY NOT NULL,
    value_json TEXT NOT NULL,
    updated_at_unix_ms INTEGER NOT NULL
);
