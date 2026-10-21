//! `orbit` — headless command-line interface for ORBIT.
//!
//! The CLI only parses arguments and formats output; every operation is implemented in
//! `orbit-core` (or `orbit-app` for `orbit ui`).

mod bench;

use orbit_core::fingerprint::FingerprintBudget;
use orbit_core::graph::Graph;
use orbit_core::history::{diff_snapshots, resolve_snapshot_ref};
use orbit_core::index::{
    index_scan, refresh, relayout, run_scan, scan_config_for, IndexOptions, IndexReport,
};
use orbit_core::relations::{combined_score, kind_label, RelationConfig};
use orbit_core::search::{search, SearchOptions};
use orbit_core::store::MAIN_CANVAS;
use orbit_core::{CancelToken, EdgeState, OrbitStore, ScanStatus};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
ORBIT — esploratore spaziale e relazionale del filesystem

Uso: orbit [--db <FILE>] <comando> [opzioni]

Comandi:
  scan <PERCORSO>            Scansiona, registra uno snapshot, aggiorna relazioni e layout
      --label <TESTO>          Etichetta dello snapshot
      --follow-symlinks        Segue i link simbolici
      --cross-mounts           Attraversa altri filesystem
      --exclude <PERCORSO>     Esclude un percorso assoluto (ripetibile)
      --no-default-excludes    Non esclude /proc, /sys, /dev, /run
      --verify-duplicates      Calcola l'hash completo dei candidati duplicati
      --no-relations           Registra solo lo snapshot
  refresh [--verify-duplicates] Ricalcola fingerprint, relazioni e layout senza scansionare
  status                     Stato del database locale
  snapshot list              Elenca gli snapshot
  snapshot create <PERCORSO> Alias di `scan`
  snapshot delete <ID>       Elimina le osservazioni di uno snapshot
  snapshot prune --keep <N>  Conserva gli N snapshot più recenti per radice
  history diff <BASE> <DEST> Confronta due snapshot (ID, `latest`, `previous`)
      --limit <N>              Elementi per categoria (default 50)
  search \"<QUERY>\"           Ricerca (sintassi: docs/ARCHITECTURE.md#ricerca)
      --limit <N>              Numero massimo di risultati (default 20)
  entity <ID>                Dettaglio di un'entità con relazioni e cronologia
  relations explain <A> <B>  Spiega le relazioni tra due entità
  relations add <A> <B> [--note <TESTO>]  Crea un'associazione manuale
  relations hide <ARCO>      Nasconde una relazione automatica
  relations show <ARCO>      Mostra di nuovo una relazione automatica nascosta
  relations delete <ARCO>    Elimina (reversibile) una relazione manuale
  relations restore <ARCO>   Ripristina una relazione manuale eliminata
  graph export --format json [--output <FILE>]  Esporta il grafo
  layout reset [--seed <N>]  Ricalcola il layout (i nodi fissati restano dove sono)
  recover [--limit <N>]      Report informativo sullo spazio (non elimina nulla)
  ui [PERCORSO] [--port <N>] [--headless] [--no-window]  Avvia l'interfaccia grafica locale
  bench [--files <N>] [--dir <DIR>]  Benchmark su un dataset sintetico (default 100000 file)

Opzioni globali:
  --db <FILE>   Database SQLite (default: $ORBIT_DB o ~/.local/share/orbit/orbit.db)
  --json        Output JSON (dove supportato)
  -h, --help    Mostra questo aiuto
";

struct Args {
    rest: Vec<String>,
    json: bool,
    db: Option<PathBuf>,
}

impl Args {
    fn parse() -> Result<Self, String> {
        let mut rest = Vec::new();
        let mut json = false;
        let mut db = None;
        let mut it = std::env::args().skip(1);
        while let Some(a) = it.next() {
            match a.as_str() {
                "--json" => json = true,
                "--db" => db = Some(PathBuf::from(it.next().ok_or("--db richiede un percorso")?)),
                _ => rest.push(a),
            }
        }
        Ok(Self { rest, json, db })
    }

    fn take_flag(&mut self, flag: &str) -> bool {
        if let Some(i) = self.rest.iter().position(|a| a == flag) {
            self.rest.remove(i);
            true
        } else {
            false
        }
    }

    fn take_value(&mut self, flag: &str) -> Result<Option<String>, String> {
        if let Some(i) = self.rest.iter().position(|a| a == flag) {
            if i + 1 >= self.rest.len() {
                return Err(format!("{} richiede un valore", flag));
            }
            let v = self.rest.remove(i + 1);
            self.rest.remove(i);
            Ok(Some(v))
        } else {
            Ok(None)
        }
    }

    fn take_values(&mut self, flag: &str) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        while let Some(v) = self.take_value(flag)? {
            out.push(v);
        }
        Ok(out)
    }

    fn positional(&self, idx: usize, what: &str) -> Result<&str, String> {
        self.rest
            .get(idx)
            .map(String::as_str)
            .ok_or_else(|| format!("argomento mancante: {}", what))
    }

    fn ensure_no_unknown(&self, max_positional: usize) -> Result<(), String> {
        if let Some(bad) = self.rest.iter().find(|a| a.starts_with("--")) {
            return Err(format!("opzione non riconosciuta: {}", bad));
        }
        if self.rest.len() > max_positional {
            return Err(format!("argomento inatteso: {}", self.rest[max_positional]));
        }
        Ok(())
    }
}

fn parse_id(s: &str, what: &str) -> Result<i64, String> {
    s.parse::<i64>()
        .map_err(|_| format!("{} non valido: {}", what, s))
}

fn print_json<T: serde::Serialize>(v: &T) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).unwrap_or_else(|_| "{}".into())
    );
}

pub(crate) fn human_bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} B", b)
    } else {
        format!("{:.1} {}", v, UNITS[u])
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn index_options(args: &mut Args) -> IndexOptions {
    let verify = args.take_flag("--verify-duplicates");
    let skip = args.take_flag("--no-relations");
    IndexOptions {
        fingerprint: FingerprintBudget {
            verify_full: verify,
            ..FingerprintBudget::default()
        },
        skip_relations: skip,
        ..IndexOptions::default()
    }
}

fn print_index_report(r: &IndexReport) {
    if let Some(rec) = &r.record {
        let s = &rec.snapshot;
        println!(
            "Snapshot #{} di {} — stato: {}",
            s.id, s.root_path_display, s.status
        );
        println!(
            "  {} file, {} directory, {} ({} allocati)",
            s.total_files_count,
            s.total_dirs_count,
            human_bytes(s.total_apparent_bytes),
            human_bytes(s.total_allocated_bytes)
        );
        println!(
            "  identità: {} nuove, {} riconosciute ({} spostate), {} con corrispondenze incerte",
            s.new_entities_count,
            s.matched_entities_count,
            s.moved_entities_count,
            s.ambiguous_entries_count
        );
        println!(
            "  non osservate: {} · probabilmente eliminate: {}",
            s.unobserved_entities_count, s.probably_deleted_entities_count
        );
        if s.permission_denied_count + s.io_errors_count > 0 {
            println!(
                "  attenzione: {} permessi negati, {} errori di I/O (scansione parziale, nessuna assenza dedotta da lì)",
                s.permission_denied_count, s.io_errors_count
            );
        }
    }
    let f = &r.fingerprints;
    println!(
        "Fingerprint: {} candidati, {} dalla cache, {} campionati, {} hash completi, {} letti",
        f.candidate_files,
        f.reused_from_cache,
        f.sampled,
        f.fully_hashed,
        human_bytes(f.bytes_read)
    );
    if f.skipped_changed_since_scan + f.read_errors + f.skipped_budget > 0 {
        println!(
            "  saltati: {} modificati dopo la scansione, {} errori di lettura, {} oltre il budget",
            f.skipped_changed_since_scan, f.read_errors, f.skipped_budget
        );
    }
    let rf = &r.references;
    println!(
        "Riferimenti: {} sorgenti, {} analizzati, {} riferimenti trovati ({} binari, {} troppo grandi)",
        rf.source_files, rf.parsed, rf.references_found, rf.skipped_binary, rf.skipped_too_large
    );
    let kinds: Vec<String> = r
        .relations
        .edges_by_kind
        .iter()
        .map(|(k, v)| format!("{} {}", v, k))
        .collect();
    println!(
        "Relazioni calcolate: {}",
        if kinds.is_empty() {
            "nessuna".to_string()
        } else {
            kinds.join(", ")
        }
    );
    println!(
        "  persistenza: {} nuove, {} aggiornate, {} invariate, {} obsolete, {} nascoste mantenute",
        r.edges.inserted,
        r.edges.updated,
        r.edges.unchanged,
        r.edges.marked_stale,
        r.edges.kept_hidden
    );
    println!(
        "Layout: {} nodi collocati, {} ricollocati dopo uno spostamento, {} invariati, {} bloccati",
        r.layout.placed_new,
        r.layout.replaced_after_move,
        r.layout.kept_existing,
        r.layout.kept_locked
    );
    let t: Vec<String> = r
        .timings_ms
        .iter()
        .map(|(k, v)| format!("{} {} ms", k, v))
        .collect();
    println!("Tempi: {}", t.join(", "));
}

fn cmd_scan(store: &OrbitStore, args: &mut Args) -> Result<(), String> {
    let label = args.take_value("--label")?;
    let follow = args.take_flag("--follow-symlinks");
    let cross = args.take_flag("--cross-mounts");
    let no_default = args.take_flag("--no-default-excludes");
    let excludes = args.take_values("--exclude")?;
    let opts = index_options(args);
    args.ensure_no_unknown(2)?;
    let path = args.positional(1, "PERCORSO")?;
    let mut cfg =
        scan_config_for(std::path::Path::new(path)).map_err(|e| format!("{}: {}", path, e))?;
    cfg.follow_symlinks = follow;
    cfg.cross_mounts = cross;
    if no_default {
        cfg.excluded_paths.clear();
    }
    for e in excludes {
        let p = PathBuf::from(&e);
        if !p.is_absolute() {
            return Err(format!("--exclude richiede un percorso assoluto: {}", e));
        }
        cfg.excluded_paths.push(p);
    }
    let json = args.json;
    if !json {
        eprintln!("Scansione di {} ...", cfg.root_path.display());
    }
    let scan = run_scan(&cfg, &CancelToken::new(), |p| {
        if !json {
            eprint!(
                "\r  {} directory, {} file, {} errori      ",
                p.dirs_scanned, p.files_scanned, p.errors_count
            );
        }
    });
    if !json {
        eprintln!();
    }
    if scan.status == ScanStatus::Failed {
        return Err(format!(
            "scansione fallita: {}",
            scan.root_error_message
                .unwrap_or_else(|| "errore sconosciuto".into())
        ));
    }
    let report = index_scan(store, &scan, label.as_deref(), &opts).map_err(|e| e.to_string())?;
    if json {
        print_json(&report);
    } else {
        print_index_report(&report);
    }
    Ok(())
}

fn cmd_refresh(store: &OrbitStore, args: &mut Args) -> Result<(), String> {
    let opts = index_options(args);
    args.ensure_no_unknown(1)?;
    let report = refresh(store, &opts).map_err(|e| e.to_string())?;
    if args.json {
        print_json(&report);
    } else {
        print_index_report(&report);
    }
    Ok(())
}

fn cmd_status(store: &OrbitStore, args: &Args) -> Result<(), String> {
    let s = store.status().map_err(|e| e.to_string())?;
    if args.json {
        print_json(&s);
        return Ok(());
    }
    println!("Database: {} (schema v{})", s.db_path, s.schema_version);
    println!("Snapshot: {}", s.snapshots);
    println!(
        "Entità: {} presenti, {} non osservate, {} probabilmente eliminate",
        s.entities_present, s.entities_unobserved, s.entities_probably_deleted
    );
    println!("Relazioni attive per tipo:");
    for (k, v) in &s.edges_by_kind {
        println!("  {:<20} {}", k, v);
    }
    println!(
        "Relazioni manuali: {} · nascoste: {} · obsolete: {}",
        s.manual_edges, s.hidden_edges, s.stale_edges
    );
    println!(
        "Fingerprint: {} ({} con contenuto verificato)",
        s.fingerprints, s.verified_hashes
    );
    println!(
        "Posizioni: {} ({} fissate) · costellazioni: {}",
        s.positions, s.pinned_positions, s.constellations
    );
    Ok(())
}

fn cmd_snapshot(store: &OrbitStore, args: &mut Args) -> Result<(), String> {
    let sub = args.positional(1, "sottocomando snapshot")?.to_string();
    match sub.as_str() {
        "list" => {
            args.ensure_no_unknown(2)?;
            let list = store.list_snapshots().map_err(|e| e.to_string())?;
            if args.json {
                print_json(&list);
            } else if list.is_empty() {
                println!("Nessuno snapshot.");
            } else {
                println!(
                    "{:>5}  {:<11} {:>9} {:>10}  {:<19} radice",
                    "ID", "stato", "file", "dimensione", "data (UTC)"
                );
                for s in list {
                    println!(
                        "{:>5}  {:<11} {:>9} {:>10}  {:<19} {}{}",
                        s.id,
                        s.status,
                        s.total_files_count,
                        human_bytes(s.total_apparent_bytes),
                        format_ts(s.created_at_unix_ms / 1000),
                        s.root_path_display,
                        s.label.map(|l| format!(" [{}]", l)).unwrap_or_default()
                    );
                }
            }
            Ok(())
        }
        "create" => {
            args.rest.remove(0);
            cmd_scan(store, args)
        }
        "delete" => {
            args.ensure_no_unknown(3)?;
            let id = parse_id(args.positional(2, "ID")?, "ID snapshot")?;
            let ok = store.delete_snapshot(id).map_err(|e| e.to_string())?;
            println!(
                "{}",
                if ok {
                    "Snapshot eliminato (entità, posizioni e relazioni conservate)."
                } else {
                    "Snapshot non trovato."
                }
            );
            Ok(())
        }
        "prune" => {
            let keep = args
                .take_value("--keep")?
                .ok_or("snapshot prune richiede --keep <N>")?
                .parse::<usize>()
                .map_err(|_| "--keep richiede un numero")?;
            args.ensure_no_unknown(2)?;
            let deleted = store.prune_snapshots(keep).map_err(|e| e.to_string())?;
            println!("Eliminati {} snapshot: {:?}", deleted.len(), deleted);
            Ok(())
        }
        other => Err(format!("sottocomando snapshot sconosciuto: {}", other)),
    }
}

fn format_ts(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn cmd_history(store: &OrbitStore, args: &mut Args) -> Result<(), String> {
    let limit = args
        .take_value("--limit")?
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|_| "--limit richiede un numero")?
        .unwrap_or(50);
    args.ensure_no_unknown(4)?;
    if args.positional(1, "diff")? != "diff" {
        return Err("uso: orbit history diff <BASE> <DEST>".into());
    }
    let base =
        resolve_snapshot_ref(store, args.positional(2, "BASE")?).map_err(|e| e.to_string())?;
    let target =
        resolve_snapshot_ref(store, args.positional(3, "DEST")?).map_err(|e| e.to_string())?;
    let d = diff_snapshots(store, base, target, limit).map_err(|e| e.to_string())?;
    if args.json {
        print_json(&d);
        return Ok(());
    }
    println!("Confronto snapshot #{} → #{}", d.base.id, d.target.id);
    println!(
        "Spostati {} · modificati {} · nuovi {} · probabilmente eliminati {} · non osservati {} · fuori radice {}",
        d.totals.moved, d.totals.modified, d.totals.appeared, d.totals.disappeared_probably_deleted,
        d.totals.disappeared_unobserved, d.totals.disappeared_outside_root
    );
    for m in &d.moved {
        println!(
            "  ↪ {} → {}{}",
            m.from_path,
            m.to_path,
            if m.content_changed {
                " (contenuto modificato)"
            } else {
                ""
            }
        );
    }
    for m in &d.modified {
        println!(
            "  ✎ {} ({} → {})",
            m.path,
            human_bytes(m.before_bytes),
            human_bytes(m.after_bytes)
        );
    }
    for a in &d.appeared {
        println!("  + {}", a.path);
    }
    for x in &d.disappeared {
        println!("  − {} [{}: {}]", x.path, x.verdict, x.reason);
    }
    if d.truncated {
        println!("(elenchi troncati a {} elementi per categoria)", limit);
    }
    Ok(())
}

fn load_graph(store: &OrbitStore) -> Result<Graph, String> {
    let canvas = store.canvas_id(MAIN_CANVAS).map_err(|e| e.to_string())?;
    Graph::load(store, canvas, RelationConfig::default()).map_err(|e| e.to_string())
}

fn cmd_search(store: &OrbitStore, args: &mut Args) -> Result<(), String> {
    let limit = args
        .take_value("--limit")?
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|_| "--limit richiede un numero")?
        .unwrap_or(20);
    let query = args.rest[1..].join(" ");
    if query.trim().is_empty() {
        return Err("uso: orbit search \"<QUERY>\"".into());
    }
    let g = load_graph(store)?;
    let r = search(
        &g,
        &query,
        &SearchOptions {
            limit,
            ..SearchOptions::default()
        },
        now_secs(),
    );
    if args.json {
        print_json(&r);
        return Ok(());
    }
    for w in &r.parsed.warnings {
        eprintln!("avviso: {}", w);
    }
    println!(
        "{} risultati per «{}» ({} µs){}",
        r.total_matches,
        r.query,
        r.elapsed_us,
        if r.truncated {
            ", mostrati i primi"
        } else {
            ""
        }
    );
    for res in &r.results {
        println!("{:>8}  {:.3}  {}", res.entity_id, res.match_score, res.path);
        println!("          {}", res.matched_reasons.join(" · "));
        if !res.relation_neighbors.is_empty() {
            let nb: Vec<String> = res
                .relation_neighbors
                .iter()
                .zip(&res.neighbor_scores)
                .filter_map(|(id, s)| g.node(*id).map(|n| format!("{} ({:.2})", n.name, s)))
                .collect();
            println!("          vicini: {}", nb.join(", "));
        }
    }
    Ok(())
}

fn cmd_entity(store: &OrbitStore, args: &Args) -> Result<(), String> {
    args.ensure_no_unknown(2)?;
    let id = parse_id(args.positional(1, "ID")?, "ID entità")?;
    let g = load_graph(store)?;
    let d = g.entity_detail(store, id).map_err(|e| e.to_string())?;
    if args.json {
        print_json(&d);
        return Ok(());
    }
    let n = &d.node;
    println!(
        "#{} {} [{:?}, {:?}]",
        n.entity_id, n.path_display, n.kind, n.state
    );
    println!(
        "  dimensione {} · modificato {}",
        human_bytes(n.display_bytes()),
        n.mtime_sec.map(format_ts).unwrap_or_else(|| "?".into())
    );
    if let Some(fp) = &d.fingerprint {
        println!(
            "  fingerprint {}… {}{}",
            &fp.sample_hash[..16.min(fp.sample_hash.len())],
            if fp.full_hash.is_some() || fp.sample_covers_full_file {
                "(contenuto verificato)"
            } else {
                "(campione)"
            },
            if fp.valid_for_current_version {
                ""
            } else {
                " — obsoleto"
            }
        );
    }
    println!("Relazioni ({}):", d.relations.len());
    for r in &d.relations {
        println!(
            "  [{}] {:<22} {:.2} conf {:.2} {} {}{}",
            r.edge_id
                .map(|i| i.to_string())
                .unwrap_or_else(|| "-".into()),
            r.kind_label,
            r.weight,
            r.confidence,
            if r.direction == "incoming" {
                "←"
            } else {
                "→"
            },
            r.other_path,
            if r.state != EdgeState::Active {
                format!(" ({:?})", r.state)
            } else {
                String::new()
            }
        );
        for reason in &r.reasons {
            println!("        · {}", reason);
        }
    }
    println!("Cronologia ({} osservazioni):", d.observations.len());
    for o in &d.observations {
        println!(
            "  #{} {} {:?} ({:.2}) {}",
            o.snapshot_id,
            format_ts(o.snapshot_created_at_unix_ms / 1000),
            o.identity_match,
            o.identity_confidence,
            o.path_display
        );
    }
    for c in &d.identity_candidates {
        println!(
            "  possibile identità con #{} {} ({:.2}): {}",
            c.candidate_entity_id, c.candidate_path_display, c.confidence, c.reason
        );
    }
    Ok(())
}

fn cmd_relations(store: &OrbitStore, args: &mut Args) -> Result<(), String> {
    let note = args.take_value("--note")?;
    let sub = args.positional(1, "sottocomando relations")?.to_string();
    match sub.as_str() {
        "explain" => {
            args.ensure_no_unknown(4)?;
            let a = parse_id(args.positional(2, "A")?, "ID entità")?;
            let b = parse_id(args.positional(3, "B")?, "ID entità")?;
            let edges: Vec<_> = store
                .load_edges(None)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|e| (e.src == a && e.dst == b) || (e.src == b && e.dst == a))
                .collect();
            let score = combined_score(&edges, &RelationConfig::default());
            if args.json {
                print_json(
                    &serde_json::json!({ "a": a, "b": b, "combined_score": score, "edges": edges }),
                );
                return Ok(());
            }
            if edges.is_empty() {
                println!("Nessuna relazione registrata tra #{} e #{}.", a, b);
            }
            println!("Punteggio combinato (euristico): {:.3}", score);
            for e in edges {
                println!(
                    "[{}] {} · {} · peso {:.2} · confidenza {:.2} · {:?} · {}",
                    e.id.unwrap_or(0),
                    kind_label(e.kind),
                    e.origin.as_db(),
                    e.weight,
                    e.confidence,
                    e.state,
                    e.provenance
                );
                for r in e.reasons {
                    println!("    · {}", r);
                }
            }
            Ok(())
        }
        "add" => {
            args.ensure_no_unknown(4)?;
            let a = parse_id(args.positional(2, "A")?, "ID entità")?;
            let b = parse_id(args.positional(3, "B")?, "ID entità")?;
            let e = store
                .create_manual_edge(a, b, note.as_deref())
                .map_err(|e| e.to_string())?;
            println!(
                "Associazione manuale #{} creata tra #{} e #{}.",
                e.id.unwrap_or(0),
                e.src,
                e.dst
            );
            Ok(())
        }
        "hide" | "show" | "delete" | "restore" => {
            args.ensure_no_unknown(3)?;
            let id = parse_id(args.positional(2, "ARCO")?, "ID relazione")?;
            let state = match sub.as_str() {
                "hide" => EdgeState::Hidden,
                "delete" => EdgeState::Deleted,
                _ => EdgeState::Active,
            };
            let e = store.set_edge_state(id, state).map_err(|e| e.to_string())?;
            println!("Relazione #{} ora {:?}.", id, e.state);
            Ok(())
        }
        other => Err(format!("sottocomando relations sconosciuto: {}", other)),
    }
}

fn cmd_graph(store: &OrbitStore, args: &mut Args) -> Result<(), String> {
    let format = args
        .take_value("--format")?
        .unwrap_or_else(|| "json".into());
    let output = args.take_value("--output")?;
    args.ensure_no_unknown(2)?;
    if args.positional(1, "export")? != "export" {
        return Err("uso: orbit graph export --format json [--output FILE]".into());
    }
    if format != "json" {
        return Err(format!(
            "formato non supportato: {} (disponibile: json)",
            format
        ));
    }
    let g = load_graph(store)?;
    let text = serde_json::to_string_pretty(&g.export()).map_err(|e| e.to_string())?;
    match output {
        Some(path) => {
            std::fs::write(&path, text).map_err(|e| format!("{}: {}", path, e))?;
            eprintln!("Grafo esportato in {}", path);
        }
        None => println!("{}", text),
    }
    Ok(())
}

fn cmd_layout(store: &OrbitStore, args: &mut Args) -> Result<(), String> {
    let seed = args
        .take_value("--seed")?
        .map(|s| s.parse::<u64>())
        .transpose()
        .map_err(|_| "--seed richiede un numero")?;
    args.ensure_no_unknown(2)?;
    if args.positional(1, "reset")? != "reset" {
        return Err("uso: orbit layout reset [--seed N]".into());
    }
    let r = relayout(store, seed).map_err(|e| e.to_string())?;
    if args.json {
        print_json(&r);
    } else {
        println!(
            "Layout ricalcolato: {} nodi riposizionati, {} fissati o manuali lasciati dove sono.",
            r.placed_new, r.kept_locked
        );
    }
    Ok(())
}

fn cmd_recover(store: &OrbitStore, args: &mut Args) -> Result<(), String> {
    let limit = args
        .take_value("--limit")?
        .map(|v| v.parse::<usize>())
        .transpose()
        .map_err(|_| "--limit richiede un numero")?
        .unwrap_or(15);
    args.ensure_no_unknown(1)?;
    let g = load_graph(store)?;
    let r = orbit_core::analyzer::space_report(&g, limit);
    if args.json {
        print_json(&r);
        return Ok(());
    }
    for n in &r.notes {
        println!("ℹ {}", n);
    }
    println!("\nFile più grandi (spazio allocato rilevato):");
    for f in &r.largest_files {
        println!("  {:>10}  {}", human_bytes(f.allocated_bytes), f.path);
    }
    println!("\nGruppi di duplicati:");
    for gr in &r.duplicate_groups {
        println!(
            "  {} · {} file · rilevato {} · recuperabile {}",
            if gr.verified {
                "verificati"
            } else {
                "candidati (non verificati)"
            },
            gr.members.len(),
            human_bytes(gr.detected_bytes),
            gr.recoverable_estimate_bytes
                .map(human_bytes)
                .unwrap_or_else(|| "non stimato".into())
        );
        for m in &gr.members {
            println!("      {}", m.path);
        }
    }
    println!("\nDirectory di cache note (contenuto non verificato):");
    for c in &r.caches {
        println!(
            "  {:>10}  {} — {}",
            human_bytes(c.detected_bytes),
            c.path,
            c.note
        );
    }
    println!(
        "\nStima recuperabile da duplicati verificati: {} · rilevato in duplicati non verificati: {}",
        human_bytes(r.verified_recoverable_bytes),
        human_bytes(r.unverified_duplicate_detected_bytes)
    );
    Ok(())
}

fn cmd_ui(store: OrbitStore, args: &mut Args) -> Result<(), String> {
    let port = args
        .take_value("--port")?
        .map(|p| p.parse::<u16>())
        .transpose()
        .map_err(|_| "--port richiede un numero")?
        .unwrap_or(0);
    let headless = args.take_flag("--headless");
    let no_window = args.take_flag("--no-window");
    args.ensure_no_unknown(2)?;
    let initial = args.rest.get(1).map(PathBuf::from);
    orbit_app::run(orbit_app::RunOptions {
        store,
        port,
        initial_scan: initial,
        open_window: !headless && !no_window,
    })
    .map_err(|e| e.to_string())
}

fn run() -> Result<(), String> {
    let mut args = Args::parse()?;
    if args.rest.is_empty() || args.rest.iter().any(|a| a == "-h" || a == "--help") {
        print!("{}", USAGE);
        return Ok(());
    }
    let cmd = args.rest[0].clone();
    if cmd == "bench" {
        return bench::run(&args.rest, args.json);
    }
    let db = args.db.clone().unwrap_or_else(OrbitStore::default_path);
    let store = OrbitStore::open(&db).map_err(|e| format!("{}: {}", db.display(), e))?;
    match cmd.as_str() {
        "scan" => cmd_scan(&store, &mut args),
        "refresh" => cmd_refresh(&store, &mut args),
        "status" => cmd_status(&store, &args),
        "snapshot" => cmd_snapshot(&store, &mut args),
        "history" => cmd_history(&store, &mut args),
        "search" => cmd_search(&store, &mut args),
        "entity" => cmd_entity(&store, &args),
        "relations" => cmd_relations(&store, &mut args),
        "graph" => cmd_graph(&store, &mut args),
        "layout" => cmd_layout(&store, &mut args),
        "recover" => cmd_recover(&store, &mut args),
        "ui" => cmd_ui(store, &mut args),
        other => Err(format!("comando sconosciuto: {} (orbit --help)", other)),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("orbit: {}", e);
            ExitCode::FAILURE
        }
    }
}
