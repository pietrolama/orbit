//! Persistent identity across two or more snapshots.

mod common;

use common::*;
use orbit_core::identity::{resolve_identities, AbsenceVerdict, KnownEntity};
use orbit_core::index::IndexOptions;
use orbit_core::store::SUPPORTED_SCHEMA_VERSION;
use orbit_core::{
    scan_directory, CancelToken, DbError, EntityState, EntryType, IdentityMatch, OrbitStore,
    ScanConfig, ScanStatus,
};
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;

fn snapshot_only() -> IndexOptions {
    IndexOptions {
        skip_relations: true,
        ..IndexOptions::default()
    }
}

#[test]
fn unchanged_tree_keeps_every_identity() {
    let env = Env::new();
    write(&env.root().join("a/one.txt"), b"one");
    write(&env.root().join("b/two.txt"), b"two");
    let first = env.index_with(&snapshot_only());
    let ids: Vec<i64> = ["a", "a/one.txt", "b", "b/two.txt"]
        .iter()
        .map(|p| env.entity(p))
        .collect();

    let second = env.index_with(&snapshot_only());
    let ids2: Vec<i64> = ["a", "a/one.txt", "b", "b/two.txt"]
        .iter()
        .map(|p| env.entity(p))
        .collect();
    assert_eq!(ids, ids2);

    let s1 = first.record.unwrap().snapshot;
    let s2 = second.record.unwrap().snapshot;
    assert_eq!(s1.new_entities_count, 5); // root + 2 dirs + 2 files
    assert_eq!(s2.new_entities_count, 0);
    assert_eq!(s2.matched_entities_count, 5);
    assert_eq!(s2.moved_entities_count, 0);
    assert_eq!(s2.probably_deleted_entities_count, 0);

    let obs = env
        .store
        .entity_observations(env.entity("a/one.txt"))
        .unwrap();
    assert_eq!(obs.len(), 2);
    assert_eq!(obs[1].identity_match, IdentityMatch::SamePath);
    assert!((obs[1].identity_confidence - 1.0).abs() < 1e-9);
}

#[test]
fn rename_and_move_keep_identity_with_confidence() {
    let env = Env::new();
    write(&env.root().join("docs/contratto.pdf"), &bytes(1, 5000));
    fs::create_dir_all(env.root().join("archivio")).unwrap();
    env.index_with(&snapshot_only());
    let id = env.entity("docs/contratto.pdf");

    fs::rename(
        env.root().join("docs/contratto.pdf"),
        env.root().join("archivio/contratto-firmato.pdf"),
    )
    .unwrap();
    let rep = env.index_with(&snapshot_only());
    assert_eq!(env.entity("archivio/contratto-firmato.pdf"), id);
    assert_eq!(rep.record.unwrap().snapshot.moved_entities_count, 1);

    let obs = env.store.entity_observations(id).unwrap();
    let last = obs.last().unwrap();
    assert_eq!(last.identity_match, IdentityMatch::Moved);
    assert!(last.identity_confidence >= 0.9);
    assert!(last
        .path_display
        .ends_with("archivio/contratto-firmato.pdf"));
    // The old path is not a deleted entity: it was the same entity.
    assert!(env
        .node("docs/contratto.pdf")
        .map(|n| n.entity_id == id)
        .unwrap_or(true));
}

#[test]
fn moved_directory_keeps_identity_of_dir_and_children() {
    let env = Env::new();
    write(&env.root().join("progetto/src/main.rs"), b"fn main() {}");
    env.index_with(&snapshot_only());
    let dir = env.entity("progetto");
    let file = env.entity("progetto/src/main.rs");
    fs::create_dir_all(env.root().join("lavoro")).unwrap();
    fs::rename(
        env.root().join("progetto"),
        env.root().join("lavoro/progetto-x"),
    )
    .unwrap();
    env.index_with(&snapshot_only());
    assert_eq!(env.entity("lavoro/progetto-x"), dir);
    assert_eq!(env.entity("lavoro/progetto-x/src/main.rs"), file);
    let n = env.node("lavoro/progetto-x/src/main.rs").unwrap();
    assert_eq!(n.state, EntityState::Present);
}

#[test]
fn in_place_modification_and_atomic_replacement() {
    let env = Env::new();
    write(&env.root().join("note.md"), b"prima versione");
    write(&env.root().join("config.toml"), b"a = 1");
    env.index_with(&snapshot_only());
    let note = env.entity("note.md");
    let cfg = env.entity("config.toml");

    // In-place: same inode.
    fs::OpenOptions::new()
        .append(true)
        .open(env.root().join("note.md"))
        .unwrap();
    write(&env.root().join("note.md"), b"seconda versione, piu lunga");
    // Atomic save: write a temp file and rename over (new inode).
    write(&env.root().join(".config.toml.tmp"), b"a = 2\nb = 3");
    fs::rename(
        env.root().join(".config.toml.tmp"),
        env.root().join("config.toml"),
    )
    .unwrap();
    env.index_with(&snapshot_only());

    assert_eq!(env.entity("note.md"), note);
    assert_eq!(env.entity("config.toml"), cfg);
    let obs = env.store.entity_observations(cfg).unwrap();
    assert_eq!(
        obs.last().unwrap().identity_match,
        IdentityMatch::SamePathReplaced
    );
    assert!(obs.last().unwrap().identity_confidence < 1.0);
    let note_obs = env.store.entity_observations(note).unwrap();
    assert_ne!(note_obs[0].apparent_bytes, note_obs[1].apparent_bytes);
}

#[test]
fn deletion_after_complete_scan_is_only_probable() {
    let env = Env::new();
    write(&env.root().join("tmp/old.log"), b"x");
    write(&env.root().join("keep.txt"), b"y");
    env.index_with(&snapshot_only());
    let id = env.entity("tmp/old.log");
    fs::remove_file(env.root().join("tmp/old.log")).unwrap();
    let rep = env.index_with(&snapshot_only());
    let n = env
        .store
        .load_nodes(true)
        .unwrap()
        .into_iter()
        .find(|n| n.entity_id == id)
        .unwrap();
    assert_eq!(n.state, EntityState::ProbablyDeleted);
    assert_eq!(
        rep.record.unwrap().snapshot.probably_deleted_entities_count,
        1
    );
    // Present-only loading excludes it.
    assert!(env
        .store
        .load_nodes(false)
        .unwrap()
        .iter()
        .all(|n| n.entity_id != id));
}

#[test]
fn permission_denied_is_unobserved_not_deleted() {
    if unsafe { libc_geteuid() } == 0 {
        eprintln!("skipped: running as root, permissions are not enforced");
        return;
    }
    let env = Env::new();
    write(&env.root().join("secret/a.txt"), b"a");
    write(&env.root().join("open/b.txt"), b"b");
    env.index_with(&snapshot_only());
    let a = env.entity("secret/a.txt");

    let secret = env.root().join("secret");
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o000)).unwrap();
    let rep = env.index_with(&snapshot_only());
    fs::set_permissions(&secret, fs::Permissions::from_mode(0o755)).unwrap();

    let snap = rep.record.unwrap().snapshot;
    assert_eq!(snap.status, "partial");
    let n = env
        .store
        .load_nodes(true)
        .unwrap()
        .into_iter()
        .find(|n| n.entity_id == a)
        .unwrap();
    assert_eq!(n.state, EntityState::Unobserved);
    assert_eq!(snap.probably_deleted_entities_count, 0);

    // Once readable again, it is present with the same identity.
    env.index_with(&snapshot_only());
    assert_eq!(env.entity("secret/a.txt"), a);
    assert_eq!(
        env.node("secret/a.txt").unwrap().state,
        EntityState::Present
    );
}

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

#[test]
fn cancelled_scans_are_refused() {
    let env = Env::new();
    write(&env.root().join("f.txt"), b"x");
    let token = CancelToken::new();
    token.cancel();
    let scan = scan_directory(
        &ScanConfig::new(fs::canonicalize(env.root()).unwrap()),
        &token,
    );
    assert_eq!(scan.status, ScanStatus::Cancelled);
    assert!(matches!(
        env.store.record_scan(&scan, None),
        Err(DbError::Invalid(_))
    ));
    assert_eq!(env.store.list_snapshots().unwrap().len(), 0);
}

#[test]
fn non_utf8_and_unicode_names_round_trip() {
    let env = Env::new();
    let raw = OsString::from_vec(b"bad_\xff\xfe.bin".to_vec());
    write(&env.root().join(&raw), b"raw");
    write(&env.root().join("città/naïve 📁.txt"), b"u");
    env.index_with(&snapshot_only());
    env.index_with(&snapshot_only());
    let nodes = env.store.load_nodes(false).unwrap();
    let bad = nodes
        .iter()
        .find(|n| n.path_raw.ends_with(b"bad_\xff\xfe.bin"))
        .expect("raw path kept");
    let obs = env.store.entity_observations(bad.entity_id).unwrap();
    assert_eq!(obs.len(), 2);
    assert_eq!(obs[1].identity_match, IdentityMatch::SamePath);
    assert!(nodes.iter().any(|n| n.name == "naïve 📁.txt"));
}

#[test]
fn scanning_a_subdirectory_does_not_touch_entities_outside_it() {
    let env = Env::new();
    write(&env.root().join("a/x.txt"), b"x");
    write(&env.root().join("b/y.txt"), b"y");
    env.index_with(&snapshot_only());
    let y = env.entity("b/y.txt");
    let a_dir = env.entity("a");

    let cfg = ScanConfig::new(fs::canonicalize(env.root().join("a")).unwrap());
    let scan = scan_directory(&cfg, &CancelToken::new());
    env.store.record_scan(&scan, Some("solo a")).unwrap();
    // The sub-root reuses the existing entity and keeps its parent link.
    assert_eq!(env.entity("a"), a_dir);
    assert_eq!(env.node("b/y.txt").unwrap().entity_id, y);
    assert_eq!(env.node("b/y.txt").unwrap().state, EntityState::Present);
    assert!(env.node("a").unwrap().parent_entity_id.is_some());
}

// ---- Pure identity resolution: cases a real filesystem cannot easily produce in a test ----------

fn known(id: i64, path: &str, ino: u64, size: u64, mtime: i64) -> KnownEntity {
    KnownEntity {
        id,
        kind: EntryType::File,
        state: EntityState::Present,
        path_raw: path.as_bytes().to_vec(),
        dev: Some(1),
        ino: Some(ino),
        apparent_bytes: size,
        mtime_sec: Some(mtime),
        mtime_nsec: Some(0),
        last_seen_snapshot_id: 1,
        row_signature: 0,
    }
}

fn scan_with_files(root: &str, files: &[(&str, u64, u64, i64)]) -> orbit_core::ScanResult {
    // files: (rel path, ino, size, mtime)
    use orbit_core::{AccessStatus, EntryId, EntryNode};
    let mut entries = vec![EntryNode {
        id: EntryId(0),
        parent_id: None,
        children_ids: Vec::new(),
        rel_path_raw: Vec::new(),
        rel_path_display: String::new(),
        name_raw: b"root".to_vec(),
        name_display: "root".into(),
        is_valid_utf8: true,
        depth: 0,
        entry_type: EntryType::Directory,
        apparent_bytes: 4096,
        allocated_bytes: 4096,
        subtree_apparent_bytes: 0,
        subtree_allocated_bytes: 0,
        direct_files_apparent_bytes: 0,
        direct_files_allocated_bytes: 0,
        subtree_files_count: 0,
        subtree_dirs_count: 0,
        subtree_symlinks_count: 0,
        subtree_error_count: 0,
        dev: 1,
        ino: 2,
        nlink: 2,
        is_hardlink_duplicate: false,
        primary_hardlink_id: None,
        shared_link_paths_in_scan: 1,
        mtime_sec: Some(0),
        mtime_nsec: Some(0),
        access_status: AccessStatus::Ok,
        error_message: None,
    }];
    for (i, (rel, ino, size, mtime)) in files.iter().enumerate() {
        let mut e = entries[0].clone();
        e.id = EntryId(i + 1);
        e.parent_id = Some(EntryId(0));
        e.rel_path_raw = rel.as_bytes().to_vec();
        e.rel_path_display = rel.to_string();
        e.name_display = rel.rsplit('/').next().unwrap().to_string();
        e.name_raw = e.name_display.as_bytes().to_vec();
        e.depth = 1;
        e.entry_type = EntryType::File;
        e.apparent_bytes = *size;
        e.ino = *ino;
        e.mtime_sec = Some(*mtime);
        entries.push(e);
    }
    let mut scan = scan_directory(
        &ScanConfig::new("/nonexistent-orbit-test"),
        &CancelToken::new(),
    );
    scan.status = ScanStatus::Completed;
    scan.root_path_raw = root.as_bytes().to_vec();
    scan.root_path_display = root.to_string();
    scan.root_id = Some(EntryId(0));
    scan.entries = entries;
    scan
}

#[test]
fn inode_reuse_is_not_merged_but_recorded_as_candidate() {
    // Old file deleted, its inode reused by an unrelated file with a different name and metadata.
    let known = vec![known(10, "/r/vecchio.doc", 77, 100, 1_000)];
    let scan = scan_with_files("/r", &[("nuovo.png", 77, 9_999, 5_000)]);
    let res = resolve_identities(&scan, &known);
    let a = &res.assignments[1];
    assert_eq!(a.entity_id, None);
    assert_eq!(a.match_kind, IdentityMatch::New);
    assert_eq!(a.candidates.len(), 1);
    assert_eq!(a.candidates[0].candidate_entity_id, 10);
    assert!(a.candidates[0].confidence < 0.5);
    assert_eq!(res.absent, vec![(10, AbsenceVerdict::ProbablyDeleted)]);
}

#[test]
fn cross_device_move_matches_by_name_size_mtime_only_when_unique() {
    // Unique: different inode, same name/size/mtime → merged with reduced confidence.
    let k = vec![known(5, "/r/old/foto.jpg", 11, 4_000, 1_234)];
    let scan = scan_with_files("/r", &[("new/foto.jpg", 999, 4_000, 1_234)]);
    let res = resolve_identities(&scan, &k);
    assert_eq!(res.assignments[1].entity_id, Some(5));
    assert_eq!(
        res.assignments[1].match_kind,
        IdentityMatch::MovedByContentMetadata
    );
    assert!(res.assignments[1].confidence <= 0.7);

    // Ambiguous: two identical candidates → no merge, both recorded.
    let k = vec![
        known(5, "/r/a/foto.jpg", 11, 4_000, 1_234),
        known(6, "/r/b/foto.jpg", 12, 4_000, 1_234),
    ];
    let scan = scan_with_files("/r", &[("c/foto.jpg", 999, 4_000, 1_234)]);
    let res = resolve_identities(&scan, &k);
    assert_eq!(res.assignments[1].entity_id, None);
    assert_eq!(res.assignments[1].candidates.len(), 2);
}

#[test]
fn present_copies_in_other_roots_are_never_merged() {
    // A file present in another root with identical metadata must not be "moved" here.
    let k = vec![known(5, "/altro/foto.jpg", 11, 4_000, 1_234)];
    let scan = scan_with_files("/r", &[("foto.jpg", 999, 4_000, 1_234)]);
    let res = resolve_identities(&scan, &k);
    assert_eq!(res.assignments[1].entity_id, None);
    assert!(
        res.absent.is_empty(),
        "entities outside the root are not classified"
    );
}

#[test]
fn migrations_reopen_and_crash_recovery() {
    let db = tempfile::TempDir::new().unwrap();
    let path = db.path().join("orbit.db");
    let store = OrbitStore::open(&path).unwrap();
    assert_eq!(store.schema_version().unwrap(), SUPPORTED_SCHEMA_VERSION);
    // Simulate a crash in the middle of recording a snapshot.
    store
        .exec_sql_for_tests(
            "INSERT INTO snapshots (root_path_raw, root_path_display, created_at_unix_ms, status) VALUES (X'2f78', '/x', 1, 'saving');",
        )
        .unwrap();
    let reopened = OrbitStore::open(&path).unwrap();
    let snaps = reopened.list_snapshots().unwrap();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].status, "interrupted");
    // Interrupted snapshots are pruned first.
    assert_eq!(reopened.prune_snapshots(5).unwrap(), vec![snaps[0].id]);

    // A database written by a newer ORBIT is refused, not rewritten.
    reopened
        .exec_sql_for_tests("PRAGMA user_version = 99;")
        .unwrap();
    assert!(matches!(
        OrbitStore::open(&path),
        Err(DbError::SchemaTooNew { found: 99, .. })
    ));
}

#[test]
fn snapshot_deletion_keeps_entities_and_prune_keeps_latest() {
    let env = Env::new();
    write(&env.root().join("f.txt"), b"1");
    for _ in 0..4 {
        env.index_with(&snapshot_only());
    }
    let id = env.entity("f.txt");
    assert_eq!(env.store.list_snapshots().unwrap().len(), 4);
    let deleted = env.store.prune_snapshots(2).unwrap();
    assert_eq!(deleted.len(), 2);
    assert_eq!(env.store.list_snapshots().unwrap().len(), 2);
    assert_eq!(env.entity("f.txt"), id);
    assert_eq!(env.store.entity_observations(id).unwrap().len(), 2);
}
