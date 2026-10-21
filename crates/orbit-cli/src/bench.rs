//! `orbit bench`: reproducible benchmark on a synthetic dataset.
//!
//! Creates a synthetic tree inside a temporary directory (never touching user files), runs the
//! full pipeline twice (cold and unchanged rescan), and measures each stage. The dataset and the
//! database are removed at the end. Numbers depend on the machine and on the page cache: they are
//! reported as measured, without extrapolation.

use crate::human_bytes;
use orbit_core::graph::{Graph, RegionQuery};
use orbit_core::index::{index_scan, scan_config_for, IndexOptions};
use orbit_core::relations::RelationConfig;
use orbit_core::search::{search, SearchOptions};
use orbit_core::store::MAIN_CANVAS;
use orbit_core::{scan_directory, CancelToken, OrbitStore};
use serde::Serialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Default, Serialize)]
struct BenchReport {
    files_requested: usize,
    entries_scanned: usize,
    files_scanned: u64,
    dirs_scanned: u64,
    scan_status: String,
    dataset_bytes: u64,
    timings_ms: BTreeMap<String, f64>,
    edges_by_kind: BTreeMap<String, usize>,
    edges_total: usize,
    search_latency_ms: BTreeMap<String, f64>,
    peak_rss_kib: Option<u64>,
    cpu_threads: usize,
}

const WORDS: &[&str] = &[
    "contratto",
    "fattura",
    "relazione",
    "preventivo",
    "bilancio",
    "progetto",
    "tesi",
    "verbale",
    "presentazione",
    "budget",
    "planimetria",
    "offerta",
    "manuale",
    "specifica",
    "ricevuta",
    "inventario",
    "calendario",
    "rapporto",
    "studio",
    "analisi",
];
const OWNERS: &[&str] = &[
    "rossi",
    "bianchi",
    "verdi",
    "esposito",
    "romano",
    "colombo",
    "ricci",
    "marino",
    "greco",
    "bruno",
    "gallo",
    "conti",
    "deluca",
    "mancini",
    "costa",
    "giordano",
    "rizzo",
    "lombardi",
    "moretti",
    "barbieri",
    "fontana",
    "santoro",
    "mariani",
    "rinaldi",
    "caruso",
    "ferrara",
    "galli",
    "martini",
    "leone",
    "longo",
    "gentile",
    "martinelli",
    "vitale",
    "lombardo",
    "serra",
    "coppola",
    "desantis",
    "dangelo",
    "marchetti",
    "parisi",
];
const SUFFIXES: &[&str] = &[
    "",
    "_v2",
    "_v3",
    "_finale",
    "_old",
    "_bozza",
    "_corretto",
    " (1)",
    "_2026-03-01",
];
const EXTS: &[&str] = &[
    "pdf", "docx", "odt", "txt", "png", "jpg", "csv", "xlsx", "md", "rs",
];

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

fn set_mtime(path: &Path, secs: i64) {
    if let Ok(f) = fs::File::options().write(true).open(path) {
        let _ = f.set_modified(UNIX_EPOCH + Duration::from_secs(secs.max(1) as u64));
    }
}

/// Builds the synthetic dataset and returns its total size in bytes.
fn build_dataset(root: &Path, files: usize) -> std::io::Result<u64> {
    let mut rng = Rng(0x0BB1_7000_5EED);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let per_dir = 100usize;
    let dirs = files.div_ceil(per_dir);
    let mut total = 0u64;
    let mut dup_pool: Vec<Vec<u8>> = Vec::new();
    let mut created = 0usize;
    let mut last_created: Option<String> = None;
    for d in 0..dirs {
        // Two-level hierarchy: area / project, mixing dense and sparse clusters.
        let area = format!("area_{:02}", d % 25);
        let dir = root
            .join(&area)
            .join(format!("{}_{:04}", WORDS[d % WORDS.len()], d));
        fs::create_dir_all(&dir)?;
        // Work sessions: each directory has a base time; a few sessions touch several directories.
        let session = now - (rng.below(365 * 86_400) as i64);
        let count = per_dir.min(files - created);
        for f in 0..count {
            let w = WORDS[rng.below(WORDS.len() as u64) as usize];
            let suffix = SUFFIXES[rng.below(SUFFIXES.len() as u64) as usize];
            let ext = EXTS[rng.below(EXTS.len() as u64) as usize];
            let name = match f % 50 {
                0 => format!("città_è_ü_{}.{}", f, ext),
                _ => {
                    let owner = OWNERS[rng.below(OWNERS.len() as u64) as usize];
                    format!("{}_{}_{}{}.{}", w, owner, rng.below(400), suffix, ext)
                }
            };
            let path = dir.join(&name);
            if path.exists() {
                continue;
            }
            let size = match rng.below(100) {
                0 => 150 * 1024 + rng.below(200 * 1024) as usize,
                1..=10 => 8 * 1024 + rng.below(32 * 1024) as usize,
                _ => 64 + rng.below(4 * 1024) as usize,
            };
            let content: Vec<u8> = if !dup_pool.is_empty() && rng.below(50) == 0 {
                dup_pool[rng.below(dup_pool.len() as u64) as usize].clone()
            } else if ext == "md" {
                // Link to an existing file, usually in another directory.
                let target = last_created
                    .clone()
                    .unwrap_or_else(|| "../../mancante.txt".to_string());
                format!(
                    "# Nota {}\n\nVedi [allegato]({}) e [web](https://example.com).\n",
                    f, target
                )
                .into_bytes()
            } else {
                (0..size).map(|_| (rng.next() & 0xff) as u8).collect()
            };
            if content.len() > 4096 && dup_pool.len() < 64 && rng.below(20) == 0 {
                dup_pool.push(content.clone());
            }
            let mut fh = fs::File::create(&path)?;
            fh.write_all(&content)?;
            drop(fh);
            total += content.len() as u64;
            last_created = Some(format!(
                "../../{}/{}/{}",
                area,
                dir.file_name().unwrap().to_string_lossy(),
                name
            ));
            let jitter = if rng.below(4) == 0 {
                rng.below(600) as i64
            } else {
                rng.below(30 * 86_400) as i64
            };
            set_mtime(&path, session - jitter);
            created += 1;
        }
    }
    // A non-UTF-8 name and an unreadable directory (partial scan path).
    let raw = root.join(OsString::from_vec(b"non_utf8_\xff\xfe.bin".to_vec()));
    fs::write(&raw, b"raw")?;
    let locked = root.join("area_00/riservato");
    fs::create_dir_all(&locked)?;
    fs::write(locked.join("segreto.txt"), b"x")?;
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000))?;
    Ok(total)
}

fn peak_rss_kib() -> Option<u64> {
    let s = fs::read_to_string("/proc/self/status").ok()?;
    s.lines()
        .find(|l| l.starts_with("VmHWM:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|v| v.parse().ok())
}

fn ms(d: Duration) -> f64 {
    (d.as_secs_f64() * 1e6).round() / 1e3
}

pub fn run(args: &[String], json: bool) -> Result<(), String> {
    let mut files = 100_000usize;
    let mut base: Option<PathBuf> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--files" => {
                files = args
                    .get(i + 1)
                    .and_then(|v| v.parse().ok())
                    .ok_or("--files richiede un numero")?;
                i += 2;
            }
            "--dir" => {
                base = Some(PathBuf::from(
                    args.get(i + 1).ok_or("--dir richiede un percorso")?,
                ));
                i += 2;
            }
            other => return Err(format!("opzione bench sconosciuta: {}", other)),
        }
    }
    let base = base.unwrap_or_else(std::env::temp_dir);
    let work = base.join(format!("orbit-bench-{}", std::process::id()));
    if work.exists() {
        return Err(format!("{} esiste già", work.display()));
    }
    let data = work.join("data");
    fs::create_dir_all(&data).map_err(|e| e.to_string())?;
    let result = run_inner(&work, &data, files, json);
    // Cleanup, always: restore permissions first so removal succeeds.
    let _ = fs::set_permissions(
        data.join("area_00/riservato"),
        fs::Permissions::from_mode(0o755),
    );
    let _ = fs::remove_dir_all(&work);
    result
}

fn run_inner(work: &Path, data: &Path, files: usize, json: bool) -> Result<(), String> {
    let mut r = BenchReport {
        files_requested: files,
        cpu_threads: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        ..Default::default()
    };
    if !json {
        eprintln!(
            "Creo il dataset sintetico ({} file) in {} ...",
            files,
            data.display()
        );
    }
    let t = Instant::now();
    r.dataset_bytes = build_dataset(data, files).map_err(|e| e.to_string())?;
    r.timings_ms
        .insert("0_dataset_creation".into(), ms(t.elapsed()));

    let store = OrbitStore::open(work.join("bench.db")).map_err(|e| e.to_string())?;
    let cfg = scan_config_for(data).map_err(|e| e.to_string())?;

    let t = Instant::now();
    let scan = scan_directory(&cfg, &CancelToken::new());
    r.timings_ms.insert("1_scan".into(), ms(t.elapsed()));
    r.entries_scanned = scan.entries.len();
    r.files_scanned = scan.total_files_count;
    r.dirs_scanned = scan.total_dirs_count;
    r.scan_status = format!("{:?}", scan.status);

    let t = Instant::now();
    let rep = index_scan(&store, &scan, Some("bench"), &IndexOptions::default())
        .map_err(|e| e.to_string())?;
    r.timings_ms
        .insert("2_index_total_cold".into(), ms(t.elapsed()));
    for (k, v) in &rep.timings_ms {
        r.timings_ms.insert(format!("2_cold_{}", k), *v as f64);
    }
    r.edges_by_kind = rep.relations.edges_by_kind.clone();
    r.edges_total = r.edges_by_kind.values().sum();

    let t = Instant::now();
    let scan2 = scan_directory(&cfg, &CancelToken::new());
    r.timings_ms
        .insert("3_rescan_unchanged".into(), ms(t.elapsed()));
    let t = Instant::now();
    let rep2 = index_scan(&store, &scan2, Some("bench-2"), &IndexOptions::default())
        .map_err(|e| e.to_string())?;
    r.timings_ms
        .insert("4_index_total_warm".into(), ms(t.elapsed()));
    for (k, v) in &rep2.timings_ms {
        r.timings_ms.insert(format!("4_warm_{}", k), *v as f64);
    }

    let t = Instant::now();
    let canvas = store.canvas_id(MAIN_CANVAS).map_err(|e| e.to_string())?;
    let g = Graph::load(&store, canvas, RelationConfig::default()).map_err(|e| e.to_string())?;
    r.timings_ms.insert("5_graph_load".into(), ms(t.elapsed()));

    let t = Instant::now();
    let ov = g.overview(400, 300);
    r.timings_ms.insert("6_overview".into(), ms(t.elapsed()));
    // Viewport centred on the most populated leaf cluster, about 1/20 of the whole space wide.
    let busiest = ov
        .clusters
        .iter()
        .filter(|c| c.files < 1000)
        .max_by_key(|c| c.files);
    if let (Some(b), Some(c)) = (ov.bounds, busiest) {
        let cx = c.x;
        let cy = c.y;
        let span = (b[2] - b[0]).max(1.0) / 20.0;
        let q = RegionQuery {
            x0: cx - span,
            y0: cy - span,
            x1: cx + span,
            y1: cy + span,
            scale: 1200.0 / (2.0 * span),
            min_px: 0.6,
            max_nodes: 2500,
            max_edges: 1200,
            min_strength: 0.0,
            kinds: None,
            focus: None,
        };
        let t = Instant::now();
        let region = g.region(&q);
        r.timings_ms
            .insert("7_region_query".into(), ms(t.elapsed()));
        r.timings_ms
            .insert("7_region_nodes_returned".into(), region.nodes.len() as f64);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    for q in [
        "contratto",
        "fattura finale",
        "ext:pdf size:>100KB",
        "modified:last-30d relazione",
        "città",
    ] {
        let mut samples = Vec::new();
        for _ in 0..5 {
            let t = Instant::now();
            let _ = search(&g, q, &SearchOptions::default(), now);
            samples.push(ms(t.elapsed()));
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        r.search_latency_ms
            .insert(q.to_string(), samples[samples.len() / 2]);
    }
    r.peak_rss_kib = peak_rss_kib();

    if json {
        println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
        return Ok(());
    }
    println!(
        "Dataset: {} voci scansionate ({} file, {} directory, {}), stato scansione {}",
        r.entries_scanned,
        r.files_scanned,
        r.dirs_scanned,
        human_bytes(r.dataset_bytes),
        r.scan_status
    );
    println!("Thread disponibili: {}", r.cpu_threads);
    for (k, v) in &r.timings_ms {
        println!("  {:<32} {:>10.1}", k, v);
    }
    println!("Archi: {} totali — {:?}", r.edges_total, r.edges_by_kind);
    println!("Latenza ricerca (mediana su 5, ms):");
    for (q, v) in &r.search_latency_ms {
        println!("  {:<32} {:>8.2}", q, v);
    }
    if let Some(kib) = r.peak_rss_kib {
        println!(
            "Memoria di picco del processo (VmHWM): {}",
            human_bytes(kib * 1024)
        );
    }
    Ok(())
}
