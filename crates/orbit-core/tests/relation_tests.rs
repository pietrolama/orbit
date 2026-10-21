//! Relation engine: name genealogy, temporal affinity, fingerprints, references, manual edges.

mod common;

use common::*;
use orbit_core::fingerprint::FingerprintBudget;
use orbit_core::index::{refresh, IndexOptions};
use orbit_core::references::{
    extract_markdown, extract_plain_text, normalize_lexically, resolve_target,
};
use orbit_core::relations::{compute_auto_edges, name_root, RelationConfig, RelationInputs};
use orbit_core::{Edge, EdgeOrigin, EdgeState, EntryType, RelationKind};
use std::collections::HashMap;
use std::fs;

const NOW: i64 = 1_790_000_000;

fn compute(nodes: &[orbit_core::GraphNode]) -> Vec<Edge> {
    let refs = HashMap::new();
    let inputs = RelationInputs {
        nodes,
        fingerprints: &[],
        references: &refs,
        co_activity: &[],
        now_unix_secs: NOW,
    };
    compute_auto_edges(&inputs, &RelationConfig::default()).0
}

fn edges_of(env: &Env, kind: RelationKind) -> Vec<Edge> {
    env.store
        .load_edges(Some(&[EdgeState::Active]))
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == kind)
        .collect()
}

fn connects(e: &Edge, a: i64, b: i64) -> bool {
    (e.src == a && e.dst == b) || (e.src == b && e.dst == a)
}

#[test]
fn name_roots_strip_version_markers() {
    let r = |n: &str| name_root(n, 3).map(|r| r.root);
    assert_eq!(r("contratto.pdf").as_deref(), Some("contratto"));
    assert_eq!(r("contratto_v2.pdf").as_deref(), Some("contratto"));
    assert_eq!(r("contratto_finale.docx").as_deref(), Some("contratto"));
    assert_eq!(
        r("Contratto Finale Corretto (1).PDF").as_deref(),
        Some("contratto")
    );
    assert_eq!(r("report-2026-03-01.xlsx").as_deref(), Some("report"));
    assert_eq!(r("tesi.tar.gz").as_deref(), Some("tesi"));
    assert_eq!(name_root("tesi.tar.gz", 3).unwrap().extension, "tar.gz");
    let m = name_root("contratto_v2_finale.pdf", 3).unwrap();
    assert!(m.markers.contains(&"v2".to_string()) && m.markers.contains(&"finale".to_string()));
    // Generic or too short roots produce nothing.
    assert_eq!(r("README.md"), None);
    assert_eq!(r("index.html"), None);
    assert_eq!(r("a1.txt"), None);
    assert_eq!(r("v2.txt"), None);
}

#[test]
fn name_genealogy_links_versions_and_explains_them() {
    let nodes = vec![
        node(1, None, EntryType::Directory, "/r", 0, Some(NOW)),
        node(
            2,
            Some(1),
            EntryType::File,
            "/r/contratto.pdf",
            100,
            Some(NOW - 9000),
        ),
        node(
            3,
            Some(1),
            EntryType::File,
            "/r/contratto_v2.pdf",
            120,
            Some(NOW - 8000),
        ),
        node(
            4,
            Some(1),
            EntryType::File,
            "/r/contratto_finale.docx",
            300,
            Some(NOW - 7000),
        ),
        node(
            5,
            Some(1),
            EntryType::File,
            "/r/fattura.pdf",
            300,
            Some(NOW - 6000),
        ),
    ];
    let edges: Vec<Edge> = compute(&nodes)
        .into_iter()
        .filter(|e| e.kind == RelationKind::NameSimilarity)
        .collect();
    assert_eq!(edges.len(), 3, "{:#?}", edges);
    assert!(edges.iter().all(|e| ![e.src, e.dst].contains(&5)));
    let e = edges.iter().find(|e| connects(e, 2, 4)).unwrap();
    assert!(e.reasons.iter().any(|r| r.contains("contratto")));
    assert!(e.reasons.iter().any(|r| r.contains("estensioni diverse")));
    assert!(e.reasons.iter().any(|r| r.contains("non prova")));
    assert!(
        e.confidence < 0.9,
        "name similarity is never treated as content identity"
    );
}

#[test]
fn large_name_groups_are_bounded_and_generic_ones_skipped() {
    let mut nodes = vec![node(1, None, EntryType::Directory, "/r", 0, Some(NOW))];
    for i in 0..40 {
        nodes.push(node(
            10 + i,
            Some(1),
            EntryType::File,
            &format!("/r/scan_{:03}.png", i),
            10,
            Some(NOW - i),
        ));
    }
    for i in 0..400 {
        nodes.push(node(
            1000 + i,
            Some(1),
            EntryType::File,
            &format!("/r/frame_{:04}.png", i),
            10,
            Some(NOW - i),
        ));
    }
    let cfg = RelationConfig::default();
    let refs = HashMap::new();
    let (edges, report) = compute_auto_edges(
        &RelationInputs {
            nodes: &nodes,
            fingerprints: &[],
            references: &refs,
            co_activity: &[],
            now_unix_secs: NOW,
        },
        &cfg,
    );
    let name: Vec<&Edge> = edges
        .iter()
        .filter(|e| e.kind == RelationKind::NameSimilarity)
        .collect();
    // 40-file group → chain of 39 edges (not 780 pairs); 400-file group skipped as generic.
    assert_eq!(name.len(), 39);
    assert_eq!(report.name_groups_generic_skipped, 1);
}

#[test]
fn temporal_affinity_rules() {
    let t = NOW - 100_000;
    let nodes = vec![
        node(1, None, EntryType::Directory, "/r", 0, Some(t)),
        node(2, Some(1), EntryType::Directory, "/r/a", 0, Some(t)),
        node(3, Some(1), EntryType::Directory, "/r/b", 0, Some(t)),
        node(10, Some(2), EntryType::File, "/r/a/bozza.odt", 10, Some(t)),
        node(
            11,
            Some(3),
            EntryType::File,
            "/r/b/grafico.png",
            10,
            Some(t + 240),
        ),
        // Same directory as 10: physical relation already, no temporal edge.
        node(
            12,
            Some(2),
            EntryType::File,
            "/r/a/appunti.odt",
            10,
            Some(t + 60),
        ),
        // Missing and future timestamps are never trusted.
        node(
            13,
            Some(3),
            EntryType::File,
            "/r/b/sconosciuto.bin",
            10,
            None,
        ),
        node(
            14,
            Some(3),
            EntryType::File,
            "/r/b/futuro.bin",
            10,
            Some(NOW + 10 * 86_400),
        ),
        // Far in time.
        node(
            15,
            Some(3),
            EntryType::File,
            "/r/b/lontano.txt",
            10,
            Some(t - 50_000),
        ),
    ];
    let edges: Vec<Edge> = compute(&nodes)
        .into_iter()
        .filter(|e| e.kind == RelationKind::TemporalAffinity)
        .collect();
    assert!(edges.iter().any(|e| connects(e, 10, 11)));
    assert!(!edges.iter().any(|e| connects(e, 10, 12)));
    assert!(!edges
        .iter()
        .any(|e| [e.src, e.dst].iter().any(|x| [13, 14, 15].contains(x))));
    let e = edges.iter().find(|e| connects(e, 10, 11)).unwrap();
    assert!(
        e.confidence <= 0.4,
        "temporal proximity alone is weak evidence"
    );
    assert!(e.reasons[0].contains("4 minuti"));
    assert!((e.weight - (-240.0f64 / 1800.0).exp()).abs() < 1e-9);
}

#[test]
fn temporal_bursts_are_ignored() {
    // 200 files extracted at the same second in different dirs: a bulk operation, not affinity.
    let mut nodes = vec![node(1, None, EntryType::Directory, "/r", 0, Some(NOW))];
    for d in 0..20 {
        nodes.push(node(
            100 + d,
            Some(1),
            EntryType::Directory,
            &format!("/r/d{}", d),
            0,
            Some(NOW),
        ));
        for f in 0..10 {
            let id = 1000 + d * 10 + f;
            nodes.push(node(
                id,
                Some(100 + d),
                EntryType::File,
                &format!("/r/d{}/f{}.dat", d, f),
                10,
                Some(NOW - 5000),
            ));
        }
    }
    let edges = compute(&nodes);
    assert!(edges
        .iter()
        .all(|e| e.kind != RelationKind::TemporalAffinity));
}

#[test]
fn edges_are_deterministic_sorted_and_without_self_loops() {
    let mut nodes = vec![node(1, None, EntryType::Directory, "/r", 0, Some(NOW))];
    for d in 0..30 {
        nodes.push(node(
            100 + d,
            Some(1),
            EntryType::Directory,
            &format!("/r/dir{}", d),
            0,
            Some(NOW),
        ));
        for f in 0..5 {
            let id = 1000 + d * 10 + f;
            let name = format!("/r/dir{}/relazione_{}_v{}.txt", d, d % 7, f);
            nodes.push(node(
                id,
                Some(100 + d),
                EntryType::File,
                &name,
                100 + f as u64,
                Some(NOW - (d * 600 + f * 37)),
            ));
        }
    }
    let a = compute(&nodes);
    let mut shuffled = nodes.clone();
    shuffled.reverse();
    let b = compute(&shuffled);
    assert_eq!(a, b, "input order must not change the output");
    assert!(a.iter().all(|e| e.src != e.dst));
    let keys: Vec<_> = a.iter().map(|e| (e.src, e.dst, e.kind)).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(keys, sorted, "sorted and unique by (src, dst, kind)");
    for e in &a {
        assert!((0.0..=1.0).contains(&e.weight) && (0.0..=1.0).contains(&e.confidence));
        assert!(!e.reasons.is_empty() && !e.provenance.is_empty());
    }
}

#[test]
fn candidate_generation_stays_sparse_on_100k_nodes() {
    let mut nodes = vec![node(1, None, EntryType::Directory, "/r", 0, Some(NOW))];
    let mut id = 10;
    for d in 0..1000 {
        let dir = id;
        nodes.push(node(
            dir,
            Some(1),
            EntryType::Directory,
            &format!("/r/d{}", d),
            0,
            Some(NOW),
        ));
        id += 1;
        for f in 0..100 {
            // ~5000 distinct roots, each shared by ~20 files spread over many directories.
            let name = format!("/r/d{}/{}_v{}.txt", d, alpha((d * 100 + f) % 5000), f % 3);
            nodes.push(node(
                id,
                Some(dir),
                EntryType::File,
                &name,
                10 + (f as u64 % 13),
                Some(NOW - (d as i64 * 12_000 + f as i64 * 120)),
            ));
            id += 1;
        }
    }
    assert!(nodes.len() > 100_000);
    let started = std::time::Instant::now();
    let edges = compute(&nodes);
    let elapsed = started.elapsed();
    eprintln!(
        "100k synthetic nodes: {} edges in {:?}",
        edges.len(),
        elapsed
    );
    assert!(edges.iter().any(|e| e.kind == RelationKind::NameSimilarity));
    assert!(edges
        .iter()
        .any(|e| e.kind == RelationKind::TemporalAffinity));
    assert!(
        edges.len() <= 10 * nodes.len(),
        "edge count must stay linear: {}",
        edges.len()
    );
}

fn alpha(mut n: usize) -> String {
    let mut s = String::from("prog");
    for _ in 0..3 {
        s.push((b'a' + (n % 26) as u8) as char);
        n /= 26;
    }
    s
}

#[test]
fn fingerprints_two_stages_and_sample_collisions() {
    let env = Env::new();
    let big = bytes(7, 300 * 1024);
    write(&env.root().join("a/video.bin"), &big);
    write(&env.root().join("b/copia video.bin"), &big);
    // Same size, same first and last 64 KiB, different middle: a sample collision.
    let mut collide = big.clone();
    collide[150 * 1024] ^= 0xff;
    write(&env.root().join("c/quasi.bin"), &collide);
    // Same size, different content.
    write(&env.root().join("c/diverso.bin"), &bytes(8, 300 * 1024));
    // Small identical files: the sample covers the whole content.
    write(&env.root().join("a/logo.svg"), &bytes(9, 2000));
    write(&env.root().join("b/logo-copy.svg"), &bytes(9, 2000));

    env.index();
    let (v1, v2, q, d) = (
        env.entity("a/video.bin"),
        env.entity("b/copia video.bin"),
        env.entity("c/quasi.bin"),
        env.entity("c/diverso.bin"),
    );
    let fm = edges_of(&env, RelationKind::FingerprintMatch);
    assert!(fm.iter().any(|e| connects(e, v1, v2)));
    assert!(
        fm.iter().any(|e| connects(e, v1, q)),
        "sample collision is only a FingerprintMatch"
    );
    assert!(!fm.iter().any(|e| [e.src, e.dst].contains(&d)));
    let vd = edges_of(&env, RelationKind::VerifiedDuplicate);
    assert!(
        !vd.iter().any(|e| [e.src, e.dst].contains(&v1)),
        "no verified duplicate from a partial hash"
    );
    assert!(vd.iter().any(|e| connects(
        e,
        env.entity("a/logo.svg"),
        env.entity("b/logo-copy.svg")
    )));

    // Stage 2: explicit verification separates the true duplicate from the collision.
    let opts = IndexOptions {
        fingerprint: FingerprintBudget {
            verify_full: true,
            ..FingerprintBudget::default()
        },
        ..IndexOptions::default()
    };
    let rep = refresh(&env.store, &opts).unwrap();
    assert!(rep.fingerprints.fully_hashed >= 3);
    let vd = edges_of(&env, RelationKind::VerifiedDuplicate);
    assert!(vd.iter().any(|e| connects(e, v1, v2)));
    assert!(!vd.iter().any(|e| [e.src, e.dst].contains(&q)));
    let fm = edges_of(&env, RelationKind::FingerprintMatch);
    assert!(
        !fm.iter().any(|e| connects(e, v1, q)),
        "proven different after full hash"
    );

    // A modified file invalidates its fingerprint.
    write(
        &env.root().join("b/copia video.bin"),
        &bytes(10, 300 * 1024),
    );
    let rep = env.index();
    assert!(rep.fingerprints.sampled >= 1);
    let vd = edges_of(&env, RelationKind::VerifiedDuplicate);
    assert!(!vd.iter().any(|e| connects(e, v1, v2)));
}

#[test]
fn markdown_and_text_reference_parsing() {
    let src = b"/home/u/progetto/README.md";
    let md = "# Titolo\n\
        Vedi [specifica](docs/spec.md#sezione) e ![schema](../img/schema%20finale.png \"titolo\").\n\
        [web](https://example.com/x.md) [mail](mailto:a@b.c) [ancora](#top)\n\
        ```\n[nel codice](codice.md)\n```\n\
        [rif]: ./allegati/fattura.pdf\n\
        <file:///etc/hosts>\n";
    let refs = extract_markdown(src, md, None);
    let targets: Vec<&str> = refs.iter().map(|r| r.target_display.as_str()).collect();
    assert_eq!(
        targets,
        vec![
            "/home/u/progetto/docs/spec.md",
            "/home/u/img/schema finale.png",
            "/home/u/progetto/allegati/fattura.pdf",
            "/etc/hosts"
        ]
    );
    assert_eq!(refs[0].line, 2);
    assert_eq!(refs[1].syntax, "markdown_image");
    assert_eq!(refs[2].syntax, "markdown_definition");

    let txt = "apri ./dati/tabella.csv, poi ~/Documenti/nota.txt e /tmp/x/y.\n";
    let refs = extract_plain_text(b"/w/leggimi.txt", txt, Some(b"/home/u"));
    let targets: Vec<&str> = refs.iter().map(|r| r.target_display.as_str()).collect();
    assert_eq!(
        targets,
        vec![
            "/w/dati/tabella.csv",
            "/home/u/Documenti/nota.txt",
            "/tmp/x/y"
        ]
    );

    assert_eq!(normalize_lexically(b"/a/./b/../c//d"), b"/a/c/d".to_vec());
    assert_eq!(
        resolve_target(b"/a/b.md", "javascript:alert(1)", None),
        None
    );
    assert_eq!(
        resolve_target(b"/a/b.md", "file://remote-host/etc/x", None),
        None
    );
    assert_eq!(
        resolve_target(b"/a/b.md", "b.md", None),
        None,
        "self references are ignored"
    );
}

#[test]
fn explicit_references_become_directed_edges_and_binaries_are_skipped() {
    let env = Env::new();
    write(
        &env.root().join("note/README.md"),
        b"Vedi [dati](../dati/tabella.csv) e [manca](../non-esiste.txt)",
    );
    write(&env.root().join("dati/tabella.csv"), b"a,b\n1,2\n");
    let mut binary = b"[x](dati/tabella.csv)".to_vec();
    binary.extend_from_slice(&[0u8; 32]);
    write(&env.root().join("falso.md"), &binary);
    let rep = env.index();
    assert_eq!(rep.references.skipped_binary, 1);
    let refs = edges_of(&env, RelationKind::ExplicitReference);
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].src, env.entity("note/README.md"));
    assert_eq!(refs[0].dst, env.entity("dati/tabella.csv"));
    assert!(refs[0].reasons[0].contains("README.md"));
    assert!(refs[0].provenance.starts_with("markdown_link:line=1"));
}

#[test]
fn manual_edges_survive_recomputation_and_hidden_auto_edges_stay_hidden() {
    let env = Env::new();
    write(&env.root().join("a/preventivo.pdf"), b"p1");
    write(&env.root().join("b/preventivo_v2.pdf"), b"p22");
    write(&env.root().join("c/foto.jpg"), b"img");
    env.index();
    let (p1, p2, foto) = (
        env.entity("a/preventivo.pdf"),
        env.entity("b/preventivo_v2.pdf"),
        env.entity("c/foto.jpg"),
    );

    let manual = env
        .store
        .create_manual_edge(foto, p1, Some("foto del sopralluogo"))
        .unwrap();
    assert_eq!(manual.origin, EdgeOrigin::Manual);
    assert!(manual.reasons.iter().any(|r| r.contains("sopralluogo")));
    assert!(env.store.create_manual_edge(p1, p1, None).is_err());

    let auto = edges_of(&env, RelationKind::NameSimilarity)
        .into_iter()
        .find(|e| connects(e, p1, p2))
        .unwrap();
    env.store
        .set_edge_state(auto.id.unwrap(), EdgeState::Hidden)
        .unwrap();
    // Auto edges cannot be "deleted", manual edges cannot be "hidden".
    assert!(env
        .store
        .set_edge_state(auto.id.unwrap(), EdgeState::Deleted)
        .is_err());
    assert!(env
        .store
        .set_edge_state(manual.id.unwrap(), EdgeState::Hidden)
        .is_err());

    let rep = env.index();
    assert_eq!(rep.edges.kept_hidden, 1);
    assert_eq!(
        env.store.edge(auto.id.unwrap()).unwrap().state,
        EdgeState::Hidden
    );
    assert_eq!(
        env.store.edge(manual.id.unwrap()).unwrap().state,
        EdgeState::Active
    );

    // Soft delete and restore of the manual edge.
    env.store
        .set_edge_state(manual.id.unwrap(), EdgeState::Deleted)
        .unwrap();
    env.index();
    assert_eq!(
        env.store.edge(manual.id.unwrap()).unwrap().state,
        EdgeState::Deleted
    );
    env.store
        .set_edge_state(manual.id.unwrap(), EdgeState::Active)
        .unwrap();
    assert_eq!(
        env.store.edge(manual.id.unwrap()).unwrap().state,
        EdgeState::Active
    );

    // When the evidence disappears the auto edge becomes stale, never silently deleted.
    fs::rename(
        env.root().join("b/preventivo_v2.pdf"),
        env.root().join("b/listino.pdf"),
    )
    .unwrap();
    let rep = env.index();
    assert!(
        rep.edges.marked_stale >= 1
            || env.store.edge(auto.id.unwrap()).unwrap().state == EdgeState::Hidden
    );
    let e = env.store.edge(auto.id.unwrap()).unwrap();
    assert!(matches!(e.state, EdgeState::Hidden | EdgeState::Stale));
}

#[test]
fn recomputation_is_incremental_on_persistence() {
    let env = Env::new();
    write(&env.root().join("x/budget_2025.ods"), b"1");
    write(&env.root().join("y/budget_2026.ods"), b"22");
    let first = env.index();
    assert!(first.edges.inserted >= 1);
    let second = env.index();
    assert_eq!(second.edges.inserted, 0);
    assert_eq!(second.edges.updated, 0);
    assert_eq!(second.edges.marked_stale, 0);
    assert!(second.edges.unchanged >= 1);
    assert_eq!(
        second.fingerprints.sampled, 0,
        "cached fingerprints are reused"
    );
}
