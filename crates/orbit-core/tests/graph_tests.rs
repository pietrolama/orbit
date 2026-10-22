//! Layout determinism and stability, LOD views, and the gravitational search engine.

mod common;

use common::*;
use orbit_core::graph::{Graph, RegionQuery};
use orbit_core::layout::{relayout_all, update_layout, LayoutConfig, NodePosition, PositionSource};
use orbit_core::relations::RelationConfig;
use orbit_core::search::{parse_query, parse_size, search, SearchOptions};
use orbit_core::store::MAIN_CANVAS;
use orbit_core::{Edge, EdgeOrigin, EdgeState, EntryType, GraphNode, RelationKind};
use std::collections::HashMap;
use std::fs;

const NOW: i64 = 1_790_000_000;

fn tree(dirs: i64, files_per_dir: i64) -> Vec<GraphNode> {
    let mut root = node(1, None, EntryType::Directory, "/r", 0, Some(NOW));
    root.subtree_files_count = (dirs * files_per_dir) as u64;
    let mut nodes = vec![root];
    for d in 0..dirs {
        let did = 100 + d;
        let mut dir = node(
            did,
            Some(1),
            EntryType::Directory,
            &format!("/r/d{}", d),
            0,
            Some(NOW),
        );
        dir.subtree_files_count = files_per_dir as u64;
        nodes.push(dir);
        for f in 0..files_per_dir {
            let id = 10_000 + d * 1000 + f;
            nodes.push(node(
                id,
                Some(did),
                EntryType::File,
                &format!("/r/d{}/f{}.txt", d, f),
                (f as u64 + 1) * 5000,
                Some(NOW - f),
            ));
        }
    }
    nodes
}

fn to_map(v: Vec<NodePosition>) -> HashMap<i64, NodePosition> {
    v.into_iter().map(|p| (p.entity_id, p)).collect()
}

#[test]
fn layout_is_deterministic_for_same_graph_and_seed() {
    let nodes = tree(12, 30);
    let cfg = LayoutConfig::default();
    let a = update_layout(&nodes, &HashMap::new(), &cfg).updates;
    let mut rev = nodes.clone();
    rev.reverse();
    let b = update_layout(&rev, &HashMap::new(), &cfg).updates;
    assert_eq!(a, b);
    assert_eq!(a.len(), nodes.len());
    let other = update_layout(
        &nodes,
        &HashMap::new(),
        &LayoutConfig {
            seed: 99,
            ..cfg.clone()
        },
    )
    .updates;
    assert_ne!(a, other, "the seed is an explicit, effective parameter");
}

#[test]
fn siblings_do_not_overlap() {
    let nodes = tree(8, 60);
    let pos = to_map(update_layout(&nodes, &HashMap::new(), &LayoutConfig::default()).updates);
    for d in 0..8 {
        let kids: Vec<&NodePosition> = (0..60).map(|f| &pos[&(10_000 + d * 1000 + f)]).collect();
        for i in 0..kids.len() {
            for j in i + 1..kids.len() {
                let (a, b) = (kids[i], kids[j]);
                let dist = ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt();
                assert!(dist + 1e-6 >= a.radius + b.radius, "overlap in dir {}", d);
            }
        }
    }
}

#[test]
fn inserting_nodes_does_not_move_existing_ones() {
    let mut nodes = tree(6, 20);
    let cfg = LayoutConfig::default();
    let first = to_map(update_layout(&nodes, &HashMap::new(), &cfg).updates);
    // New files in an existing dir and a whole new directory.
    nodes.push(node(
        90_001,
        Some(102),
        EntryType::File,
        "/r/d2/nuovo.txt",
        10,
        Some(NOW),
    ));
    nodes.push(node(
        90_002,
        Some(1),
        EntryType::Directory,
        "/r/nuova",
        0,
        Some(NOW),
    ));
    nodes.push(node(
        90_003,
        Some(90_002),
        EntryType::File,
        "/r/nuova/x.txt",
        10,
        Some(NOW),
    ));
    let second = update_layout(&nodes, &first, &cfg);
    let ids: Vec<i64> = second.updates.iter().map(|p| p.entity_id).collect();
    assert_eq!(
        ids,
        vec![90_001, 90_002, 90_003],
        "only new nodes are written"
    );
    assert_eq!(second.report.placed_new, 3);
    // The new file lands near its parent directory.
    let parent = &first[&102];
    let p = &second.updates[0];
    let dist = ((p.x - parent.x).powi(2) + (p.y - parent.y).powi(2)).sqrt();
    assert!(
        dist < parent.radius * 2.5,
        "new node placed near its physical parent"
    );
}

#[test]
fn moved_nodes_follow_their_new_parent_but_locked_nodes_stay() {
    let mut nodes = tree(4, 5);
    let cfg = LayoutConfig::default();
    let mut pos = to_map(update_layout(&nodes, &HashMap::new(), &cfg).updates);
    // Pin one file; then move d1 under d0 (with its files) and move the pinned file too.
    let pinned_id = 10_000 + 3 * 1000;
    pos.get_mut(&pinned_id).unwrap().pinned = true;
    let before_pinned = pos[&pinned_id].clone();
    let before_child = pos[&(10_000 + 1000)].clone();
    let before_dir = pos[&101].clone();
    for n in nodes.iter_mut() {
        if n.entity_id == 101 {
            n.parent_entity_id = Some(100);
        }
        if n.entity_id == pinned_id {
            n.parent_entity_id = Some(100);
        }
    }
    let r = update_layout(&nodes, &pos, &cfg);
    let up = to_map(r.updates);
    assert!(!up.contains_key(&pinned_id), "pinned nodes are never moved");
    let moved_dir = &up[&101];
    let moved_child = &up[&(10_000 + 1000)];
    let (dx, dy) = (moved_dir.x - before_dir.x, moved_dir.y - before_dir.y);
    assert!(
        (moved_child.x - before_child.x - dx).abs() < 1e-9
            && (moved_child.y - before_child.y - dy).abs() < 1e-9,
        "descendants are translated with their directory"
    );
    assert_eq!(moved_dir.cluster_id, Some(100));
    assert_eq!(r.report.replaced_after_move, 1);
    assert_eq!(pos[&pinned_id], before_pinned);
}

#[test]
fn relayout_preserves_pinned_and_manual_positions() {
    let nodes = tree(3, 4);
    let cfg = LayoutConfig::default();
    let mut pos = to_map(update_layout(&nodes, &HashMap::new(), &cfg).updates);
    let manual = pos.get_mut(&10_000).unwrap();
    manual.x = 12345.0;
    manual.source = PositionSource::Manual;
    let r = relayout_all(
        &nodes,
        &pos,
        &LayoutConfig {
            seed: 7,
            layout_version: 2,
            ..cfg
        },
    );
    assert!(r.updates.iter().all(|p| p.entity_id != 10_000));
    assert_eq!(r.report.kept_locked, 1);
    assert!(r.updates.iter().all(|p| p.layout_version == 2));
}

#[test]
fn store_never_overwrites_manual_positions_with_auto_ones() {
    let env = Env::new();
    write(&env.root().join("a/x.txt"), b"x");
    env.index();
    let canvas = env.store.canvas_id(MAIN_CANVAS).unwrap();
    let id = env.entity("a/x.txt");
    let mut p = env.store.load_positions(canvas).unwrap()[&id].clone();
    p.x = 777.0;
    p.y = -5.0;
    env.store
        .save_manual_positions(canvas, std::slice::from_ref(&p))
        .unwrap();
    let mut auto = p.clone();
    auto.x = 1.0;
    env.store.save_auto_positions(canvas, &[auto]).unwrap();
    let now = env.store.load_positions(canvas).unwrap()[&id].clone();
    assert_eq!(
        (now.x, now.y, now.source),
        (777.0, -5.0, PositionSource::Manual)
    );
    // Positions survive rescans.
    env.index();
    assert_eq!(env.store.load_positions(canvas).unwrap()[&id].x, 777.0);
    // Invalid coordinates are rejected.
    let mut bad = p.clone();
    bad.x = f64::NAN;
    assert!(env.store.save_manual_positions(canvas, &[bad]).is_err());
    // Pin / release.
    env.store.set_pinned(canvas, &[id], true).unwrap();
    assert!(env.store.load_positions(canvas).unwrap()[&id].pinned);
    env.store.release_positions(canvas, &[id]).unwrap();
    let rel = env.store.load_positions(canvas).unwrap()[&id].clone();
    assert!(!rel.pinned && rel.source == PositionSource::Auto && rel.x == 777.0);
}

fn edge(src: i64, dst: i64, kind: RelationKind, w: f64, c: f64) -> Edge {
    Edge {
        id: Some(src * 100_000 + dst),
        src,
        dst,
        kind,
        origin: EdgeOrigin::Auto,
        weight: w,
        confidence: c,
        provenance: "test".into(),
        reasons: vec!["test".into()],
        state: EdgeState::Active,
    }
}

fn sample_graph() -> Graph {
    let mut nodes = tree(10, 10);
    nodes.push(node(
        50,
        Some(100),
        EntryType::File,
        "/r/d0/fattura_2026_marzo.pdf",
        2_000_000,
        Some(NOW - 3600),
    ));
    nodes.push(node(
        51,
        Some(105),
        EntryType::File,
        "/r/d5/fattura_2026_aprile.pdf",
        30_000_000,
        Some(NOW - 30 * 86_400),
    ));
    nodes.push(node(
        52,
        Some(107),
        EntryType::File,
        "/r/d7/fattura.odt",
        1000,
        Some(NOW - 2 * 86_400),
    ));
    let positions =
        to_map(update_layout(&nodes, &HashMap::new(), &LayoutConfig::default()).updates);
    let edges = vec![
        edge(50, 51, RelationKind::NameSimilarity, 0.9, 0.7),
        edge(50, 52, RelationKind::TemporalAffinity, 0.5, 0.3),
        edge(10_000, 50, RelationKind::ExplicitReference, 0.9, 0.9),
        edge(10_005, 10_006, RelationKind::VerifiedDuplicate, 1.0, 0.99),
    ];
    Graph::from_parts(
        nodes,
        edges,
        positions,
        Vec::new(),
        RelationConfig::default(),
    )
}

#[test]
fn neighbors_are_ranked_and_satellites_are_outside_the_subtree() {
    let g = sample_graph();
    let n = g.neighbors(50, 10);
    assert_eq!(
        n.iter().map(|x| x.entity_id).collect::<Vec<_>>(),
        vec![10_000, 51, 52]
    );
    assert!(n[0].score >= n[1].score && n[1].score >= n[2].score);
    // Satellites of d0: related entities physically elsewhere (d5, d7), not d0's own children.
    let sats = g.satellites(100, 10);
    let ids: Vec<i64> = sats.iter().map(|s| s.entity_id).collect();
    assert_eq!(ids, vec![51, 52]);
    assert_eq!(sats[0].via, 50);
}

#[test]
fn overview_and_region_respect_caps() {
    let g = sample_graph();
    let ov = g.overview(5, 3);
    assert!(ov.clusters.len() <= 5);
    assert!(ov.flows.len() <= 3);
    assert_eq!(ov.total_edges, 4);
    let big = g.overview(1000, 100);
    assert_eq!(big.clusters.len(), 11, "root + 10 dirs");
    // d0 ↔ d5 and d0 ↔ d7 flows exist between clusters.
    assert!(big.flows.iter().any(|f| (f.a, f.b) == (100, 105)));

    let b = ov.bounds.unwrap();
    let q = RegionQuery {
        within: None,
        x0: b[0],
        y0: b[1],
        x1: b[2],
        y1: b[3],
        scale: 100.0,
        min_px: 0.0,
        max_nodes: 20,
        max_edges: 2,
        min_strength: 0.0,
        kinds: None,
        focus: Some(50),
    };
    let r = g.region(&q);
    assert!(r.nodes.len() <= 20 + 1 + 64);
    assert!(
        r.nodes.iter().any(|n| n.id == 50),
        "focus is always included"
    );
    assert!(r.edges.len() <= 2);
    assert!(r.omitted_nodes > 0);
    // At low zoom small nodes are omitted.
    let far = RegionQuery {
        scale: 0.001,
        min_px: 0.6,
        focus: None,
        max_nodes: 5000,
        ..q
    };
    assert!(g.region(&far).nodes.len() < g.nodes().len());
}

#[test]
fn query_parsing() {
    let q = parse_query("fattura 2026 ext:pdf,DOCX size:>10MB modified:last-7d type:file name:marzo path:d0 related-to:42 bogus:", NOW);
    assert_eq!(q.terms, vec!["fattura", "2026", "bogus:"]);
    assert_eq!(q.extensions, vec!["pdf", "docx"]);
    assert_eq!(q.size.unwrap().min, Some(10 * 1024 * 1024));
    assert!(!q.size.unwrap().min_inclusive);
    assert_eq!(q.modified.unwrap().min, Some(NOW - 7 * 86_400));
    assert_eq!(q.kinds, vec![EntryType::File]);
    assert_eq!(q.name_terms, vec!["marzo"]);
    assert_eq!(q.path_terms, vec!["d0"]);
    assert_eq!(q.related_to, Some(42));
    assert_eq!(parse_size("1.5k"), Some(1536));
    let bad = parse_query("size:>lots modified:yesterday", NOW);
    assert_eq!(bad.warnings.len(), 2);
    let quoted = parse_query("\"nota spese\" x", NOW);
    assert_eq!(quoted.terms, vec!["nota spese", "x"]);
}

#[test]
fn search_ranking_is_reproducible_and_returns_neighbors() {
    let g = sample_graph();
    let opts = SearchOptions::default();
    let none = search(&g, "inesistente-zzz", &opts, NOW);
    assert_eq!(none.total_matches, 0);
    assert!(none.results.is_empty());
    let empty = search(&g, "   ", &opts, NOW);
    assert!(empty.results.is_empty());

    let one = search(&g, "marzo", &opts, NOW);
    assert_eq!(one.results.len(), 1);
    assert_eq!(one.results[0].entity_id, 50);
    assert_eq!(one.results[0].relation_neighbors, vec![10_000, 51, 52]);
    assert!(one.results[0]
        .matched_reasons
        .iter()
        .any(|r| r.contains("marzo")));

    let many = search(&g, "fattura", &opts, NOW);
    assert_eq!(many.results.len(), 3);
    let again = search(&g, "fattura", &opts, NOW);
    let ids = |r: &orbit_core::search::SearchResponse| {
        r.results.iter().map(|x| x.entity_id).collect::<Vec<_>>()
    };
    assert_eq!(ids(&many), ids(&again));
    // Prefix name matches rank above inner matches; recent files get a small boost.
    assert_eq!(ids(&many)[0], 50);

    let big = search(&g, "ext:pdf size:>10MB", &opts, NOW);
    assert_eq!(ids(&big), vec![51]);
    let recent = search(&g, "fattura modified:last-7d", &opts, NOW);
    assert_eq!(ids(&recent), vec![50, 52]);
    let rel = search(&g, "related-to:50", &opts, NOW);
    assert_eq!(ids(&rel), vec![10_000, 51, 52]);
    assert_eq!(rel.results[0].matched_fields[0], "relation");

    let lots = search(
        &g,
        "type:file",
        &SearchOptions {
            limit: 7,
            ..opts.clone()
        },
        NOW,
    );
    assert_eq!(lots.results.len(), 7);
    assert!(lots.truncated && lots.total_matches > 7);
}

#[test]
fn search_does_not_touch_persistent_state() {
    let env = Env::new();
    write(&env.root().join("x/fattura.pdf"), b"1");
    write(&env.root().join("y/fattura_v2.pdf"), b"12");
    env.index();
    let canvas = env.store.canvas_id(MAIN_CANVAS).unwrap();
    let before = env.store.load_positions(canvas).unwrap();
    let edges_before = env.store.load_edges(None).unwrap();
    let g = Graph::load(&env.store, canvas, RelationConfig::default()).unwrap();
    let r = search(&g, "fattura", &SearchOptions::default(), NOW);
    assert_eq!(r.results.len(), 2);
    assert_eq!(env.store.load_positions(canvas).unwrap(), before);
    assert_eq!(env.store.load_edges(None).unwrap(), edges_before);
    let _ = fs::metadata(env.root());
}

#[test]
fn huge_flat_directories_are_laid_out_in_linear_time() {
    // Regression: a browser cache with ~42k files in one directory made the spiral search
    // quadratic (each child rescanned the filled centre). Must now take well under a second or two.
    let mut nodes = vec![node(1, None, EntryType::Directory, "/r", 0, Some(NOW))];
    nodes.push(node(
        2,
        Some(1),
        EntryType::Directory,
        "/r/cache",
        0,
        Some(NOW),
    ));
    for f in 0..40_000 {
        nodes.push(node(
            10 + f,
            Some(2),
            EntryType::File,
            &format!("/r/cache/{:x}", f),
            4096,
            Some(NOW),
        ));
    }
    let cfg = LayoutConfig::default();
    let t = std::time::Instant::now();
    let first = to_map(update_layout(&nodes, &HashMap::new(), &cfg).updates);
    let cold = t.elapsed();
    assert_eq!(first.len(), nodes.len());
    // 5k new files in the same directory, placed incrementally next to the existing ones.
    for f in 40_000..45_000 {
        nodes.push(node(
            10 + f,
            Some(2),
            EntryType::File,
            &format!("/r/cache/{:x}", f),
            4096,
            Some(NOW),
        ));
    }
    let t = std::time::Instant::now();
    let inc = update_layout(&nodes, &first, &cfg);
    let warm = t.elapsed();
    assert_eq!(inc.report.placed_new, 5_000);
    eprintln!("40k flat dir: cold {:?}, +5k incremental {:?}", cold, warm);
    assert!(
        cold.as_secs_f64() < 10.0 && warm.as_secs_f64() < 10.0,
        "layout is not linear: {:?} / {:?}",
        cold,
        warm
    );
    // Spot-check: no overlap among a sample of the new files.
    let sample: Vec<&NodePosition> = inc.updates.iter().take(300).collect();
    for i in 0..sample.len() {
        for j in i + 1..sample.len() {
            let (a, b) = (sample[i], sample[j]);
            assert!(
                ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt() + 1e-6 >= a.radius + b.radius
            );
        }
    }
}

#[test]
fn mixed_size_children_do_not_stall_the_spiral() {
    // A home directory: a few huge subtrees next to many small files (the second huge sibling
    // used to need millions of spiral steps of the small files' size).
    let mut nodes = vec![node(1, None, EntryType::Directory, "/home", 0, Some(NOW))];
    let mut id = 10;
    for d in 0..6 {
        let dir = id;
        nodes.push(node(
            dir,
            Some(1),
            EntryType::Directory,
            &format!("/home/big{}", d),
            0,
            Some(NOW),
        ));
        id += 1;
        for f in 0..8_000 {
            nodes.push(node(
                id,
                Some(dir),
                EntryType::File,
                &format!("/home/big{}/{}", d, f),
                100,
                Some(NOW),
            ));
            id += 1;
        }
    }
    for f in 0..2_000 {
        nodes.push(node(
            id,
            Some(1),
            EntryType::File,
            &format!("/home/f{}", f),
            100,
            Some(NOW),
        ));
        id += 1;
    }
    let t = std::time::Instant::now();
    let pos = to_map(update_layout(&nodes, &HashMap::new(), &LayoutConfig::default()).updates);
    eprintln!("mixed sizes ({} nodes): {:?}", nodes.len(), t.elapsed());
    assert_eq!(pos.len(), nodes.len());
    assert!(t.elapsed().as_secs_f64() < 10.0);
    let dirs: Vec<&NodePosition> = (0..6).map(|d| &pos[&(10 + d * 8001)]).collect();
    for i in 0..dirs.len() {
        for j in i + 1..dirs.len() {
            let (a, b) = (dirs[i], dirs[j]);
            assert!(
                ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt() + 1e-6 >= a.radius + b.radius,
                "big subtrees overlap"
            );
        }
    }
}

#[test]
fn nested_single_child_directories_grow_additively() {
    // Regression: children were placed around a reserved core, so each level of nesting at least
    // doubled the disc radius; on a real home directory (depth 23) coordinates reached ~1e11 and
    // the spatial grid overflowed into an allocation that exhausted memory. A lone child now
    // takes the centre of its parent, which only adds the padding.
    let depth = 40;
    let mut nodes = vec![node(1, None, EntryType::Directory, "/d0", 0, Some(NOW))];
    let mut path = "/d0".to_string();
    for d in 1..=depth {
        path.push_str(&format!("/d{}", d));
        nodes.push(node(
            d + 1,
            Some(d),
            EntryType::Directory,
            &path,
            0,
            Some(NOW),
        ));
    }
    let cfg = LayoutConfig::default();
    let pos = to_map(update_layout(&nodes, &HashMap::new(), &cfg).updates);
    assert_eq!(pos.len(), nodes.len());
    for d in 1..=depth {
        let (outer, inner) = (&pos[&d], &pos[&(d + 1)]);
        assert!(
            outer.radius <= inner.radius + cfg.padding + 1e-9,
            "level {}: {} around a child of {}",
            d,
            outer.radius,
            inner.radius
        );
    }
    assert!(pos.values().all(|p| p.x.is_finite() && p.y.is_finite()));
    assert!(pos[&1].radius < cfg.dir_core_radius + (depth as f64 + 1.0) * cfg.padding + 1e-6);
}

fn projects_graph() -> Graph {
    let nodes = vec![
        node(1, None, EntryType::Directory, "/p", 0, Some(NOW)),
        node(2, Some(1), EntryType::Directory, "/p/a", 0, Some(NOW)),
        node(3, Some(1), EntryType::Directory, "/p/b", 0, Some(NOW - 10)),
        node(4, Some(1), EntryType::File, "/p/loose.txt", 10, Some(NOW)),
        node(5, Some(2), EntryType::Directory, "/p/a/src", 0, Some(NOW)),
        node(10, Some(5), EntryType::File, "/p/a/src/x.md", 10, Some(NOW)),
        node(
            11,
            Some(3),
            EntryType::File,
            "/p/b/y.md",
            10,
            Some(NOW - 10),
        ),
        node(
            12,
            Some(3),
            EntryType::File,
            "/p/b/z.txt",
            10,
            Some(NOW - 10),
        ),
    ];
    let positions =
        to_map(update_layout(&nodes, &HashMap::new(), &LayoutConfig::default()).updates);
    let edges = vec![
        edge(10, 11, RelationKind::ExplicitReference, 0.9, 0.9),
        edge(10, 12, RelationKind::TemporalAffinity, 0.3, 0.5),
        edge(10, 4, RelationKind::VerifiedDuplicate, 1.0, 0.99),
        // Strong name genealogy with temporal proximity is still circumstantial across projects:
        // shown only without a threshold.
        edge(10, 12, RelationKind::NameSimilarity, 0.9, 0.9),
    ];
    Graph::from_parts(
        nodes,
        edges,
        positions,
        Vec::new(),
        RelationConfig::default(),
    )
}

#[test]
fn projects_are_the_directories_under_a_root_with_their_external_links() {
    let g = projects_graph();
    assert_eq!(g.project_of(10), Some(2));
    assert_eq!(g.project_of(1), Some(1));
    let ps = g.projects(0.4);
    let ids: Vec<i64> = ps.iter().map(|p| p.id).collect();
    assert_eq!(
        ids,
        vec![2, 3],
        "most recent first, files under the root are not projects"
    );
    // Strong links: /p/b/y.md (another project) and /p/loose.txt (outside any project).
    assert_eq!((ps[0].external_links, ps[0].linked_projects), (2, 1));
    // The weak temporal pair only counts without a threshold.
    assert_eq!(g.projects(0.0)[0].external_links, 3);
}

#[test]
fn project_view_groups_external_links_and_hides_weak_ones() {
    let g = projects_graph();
    let v = g.project_view(2, 0.4, 80).unwrap();
    assert_eq!(v.root.id, 2);
    assert_eq!((v.total_links, v.weak_links), (2, 1));
    let b = v.groups.iter().find(|gr| gr.id == 3).unwrap();
    assert!(b.is_project);
    assert_eq!(b.links.len(), 1);
    assert_eq!(b.links[0].entity_id, 11);
    assert_eq!(b.links[0].via, vec![10]);
    let root = v.groups.iter().find(|gr| gr.id == 1).unwrap();
    assert!(
        !root.is_project,
        "a file directly under the root has no project"
    );
    assert_eq!(root.links[0].entity_id, 4);
    // Strongest group first: the verified duplicate beats the reference.
    assert_eq!(v.groups[0].id, 1);
    let all = g.project_view(2, 0.0, 80).unwrap();
    assert_eq!((all.total_links, all.weak_links), (3, 0));
    assert_eq!(g.project_view(2, 0.0, 1).unwrap().groups.len(), 1);
}

#[test]
fn region_and_overview_within_a_project_stay_inside_it() {
    let g = projects_graph();
    let q = RegionQuery {
        within: Some(2),
        x0: -1e9,
        y0: -1e9,
        x1: 1e9,
        y1: 1e9,
        scale: 1.0,
        min_px: 0.0,
        max_nodes: 100,
        max_edges: 100,
        min_strength: 0.0,
        kinds: None,
        focus: Some(10),
    };
    let r = g.region(&q);
    let mut ids: Vec<i64> = r.nodes.iter().map(|n| n.id).collect();
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec![2, 5, 10],
        "outside neighbours of the focus are not pulled in"
    );
    assert!(r.edges.is_empty());
    let ov = g.overview_within(300, 100, Some(2));
    assert_eq!(ov.total_nodes, 3);
    assert!(ov.clusters.iter().all(|c| [2, 5].contains(&c.id)));
    let p = g.position(2).unwrap();
    assert_eq!(
        ov.bounds.unwrap(),
        [
            p.x - p.radius,
            p.y - p.radius,
            p.x + p.radius,
            p.y + p.radius
        ]
    );
}

/// Builds `/p/<project>/<rel>` trees from file lists, creating intermediate directories.
fn projects_tree(projects: &[(&str, i64, &[&str])]) -> Vec<GraphNode> {
    let mut nodes = vec![node(1, None, EntryType::Directory, "/p", 0, Some(NOW))];
    let mut ids: HashMap<String, i64> = HashMap::from([("/p".to_string(), 1)]);
    let mut next = 2;
    for (name, mtime, files) in projects {
        for rel in files.iter() {
            let full = format!("/p/{}/{}", name, rel);
            let parts: Vec<&str> = full.split('/').skip(1).collect();
            for i in 1..parts.len() {
                let path = format!("/{}", parts[..=i].join("/"));
                if ids.contains_key(&path) {
                    continue;
                }
                let parent = ids[&format!("/{}", parts[..i].join("/"))];
                let kind = if i + 1 == parts.len() {
                    EntryType::File
                } else {
                    EntryType::Directory
                };
                nodes.push(node(next, Some(parent), kind, &path, 100, Some(*mtime)));
                ids.insert(path, next);
                next += 1;
            }
        }
    }
    nodes
}

#[test]
fn kinship_finds_a_rewritten_project_and_ignores_generic_or_generated_names() {
    let old = NOW - 30 * 86_400;
    let nodes = projects_tree(&[
        (
            "alphaFrame",
            old,
            &[
                "readme.md",
                "main.py",
                "memory/episodic.py",
                "core/registry.py",
                "build/CMakeFiles/cache.txt",
            ],
        ),
        (
            "alphaMobile",
            NOW,
            &[
                "readme.md",
                "main.py",
                "memory/episodic.py",
                "tools/registry.py",
                "build/CMakeFiles/cache.txt",
            ],
        ),
        // Same word in the name, nothing else in common.
        ("hackThis", NOW, &["readme.md", "main.py", "client.py"]),
        ("neuronHACK", NOW, &["readme.md", "main.py", "agent.py"]),
        // Only generated files in common.
        (
            "webOne",
            NOW,
            &["readme.md", "main.py", "build/CMakeFiles/cache.txt", "a.js"],
        ),
        (
            "webTwo",
            NOW,
            &["readme.md", "main.py", "build/CMakeFiles/cache.txt", "b.js"],
        ),
    ]);
    let positions =
        to_map(update_layout(&nodes, &HashMap::new(), &LayoutConfig::default()).updates);
    let g = Graph::from_parts(
        nodes,
        Vec::new(),
        positions,
        Vec::new(),
        RelationConfig::default(),
    );
    let id = |name: &str| g.nodes().iter().find(|n| n.name == name).unwrap().entity_id;
    let kin = g.kinship(0.5);
    assert_eq!(kin.len(), 1, "{:?}", kin);
    let k = &kin[0];
    assert_eq!((k.a, k.b), (id("alphaFrame"), id("alphaMobile")));
    assert_eq!(k.older, Some(id("alphaFrame")));
    assert_eq!(k.name_root.as_deref(), Some("alpha"));
    assert!(k.shared.contains(&"memory/episodic".to_string()));
    assert!(k
        .shared
        .iter()
        .all(|t| !t.contains("cache") && !t.contains("cmake")));
    assert!(
        !k.shared.contains(&"readme".to_string()),
        "names every project uses carry no signal"
    );

    let summaries = g.projects(0.4);
    let mobile = summaries.iter().find(|p| p.name == "alphaMobile").unwrap();
    assert_eq!(mobile.relatives.len(), 1);
    assert_eq!(mobile.relatives[0].came_first, Some(true));
    let view = g.project_view(id("alphaFrame"), 0.4, 80).unwrap();
    assert_eq!(view.relatives[0].name, "alphaMobile");
    assert_eq!(view.relatives[0].came_first, Some(false));
}
