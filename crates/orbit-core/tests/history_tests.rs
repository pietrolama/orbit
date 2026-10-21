//! Entity-based history between snapshots.

mod common;

use common::*;
use orbit_core::history::{diff_snapshots, resolve_snapshot_ref};
use orbit_core::index::IndexOptions;
use std::fs;
use std::os::unix::fs::PermissionsExt;

fn opts() -> IndexOptions {
    IndexOptions {
        skip_relations: true,
        ..IndexOptions::default()
    }
}

#[test]
fn diff_distinguishes_moves_modifications_new_and_missing() {
    let env = Env::new();
    write(&env.root().join("docs/a.txt"), b"a");
    write(&env.root().join("docs/b.txt"), b"b");
    write(&env.root().join("docs/c.txt"), b"c");
    fs::create_dir_all(env.root().join("archivio")).unwrap();
    let base = env.index_with(&opts()).record.unwrap().snapshot.id;

    fs::rename(
        env.root().join("docs/a.txt"),
        env.root().join("archivio/a.txt"),
    )
    .unwrap();
    write(&env.root().join("docs/b.txt"), b"b modificato");
    fs::remove_file(env.root().join("docs/c.txt")).unwrap();
    write(&env.root().join("docs/d.txt"), b"d");
    let target = env.index_with(&opts()).record.unwrap().snapshot.id;

    let d = diff_snapshots(&env.store, base, target, 100).unwrap();
    assert_eq!(d.totals.moved, 1);
    assert!(
        d.moved[0].from_path.ends_with("docs/a.txt")
            && d.moved[0].to_path.ends_with("archivio/a.txt")
    );
    assert!(!d.moved[0].content_changed);
    assert_eq!(d.totals.modified, 1);
    assert!(d.modified[0].path.ends_with("docs/b.txt"));
    assert_eq!(d.totals.appeared, 1);
    assert!(d.appeared[0].path.ends_with("docs/d.txt"));
    assert_eq!(d.totals.disappeared_probably_deleted, 1);
    assert_eq!(d.disappeared[0].verdict, "probably_deleted");
    assert!(!d.truncated);

    assert_eq!(resolve_snapshot_ref(&env.store, "latest").unwrap(), target);
    assert_eq!(resolve_snapshot_ref(&env.store, "previous").unwrap(), base);
}

#[test]
fn diff_against_partial_snapshot_never_claims_deletion() {
    if unsafe { geteuid() } == 0 {
        eprintln!("skipped: running as root");
        return;
    }
    let env = Env::new();
    write(&env.root().join("chiuso/x.txt"), b"x");
    write(&env.root().join("aperto/y.txt"), b"y");
    let base = env.index_with(&opts()).record.unwrap().snapshot.id;
    let locked = env.root().join("chiuso");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let target = env.index_with(&opts()).record.unwrap().snapshot.id;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let d = diff_snapshots(&env.store, base, target, 100).unwrap();
    assert_eq!(d.totals.disappeared_probably_deleted, 0);
    assert_eq!(d.totals.disappeared_unobserved, 1);
    assert_eq!(d.disappeared[0].verdict, "unobserved");
}

extern "C" {
    fn geteuid() -> u32;
}
