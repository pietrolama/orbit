//! `orbit-app`: starts the local ORBIT interface. Equivalent to `orbit ui`.

use orbit_app::{run, RunOptions};
use orbit_core::OrbitStore;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "Uso: orbit-app [PERCORSO] [--db FILE] [--port N] [--headless]\n\
  PERCORSO     directory da scansionare all'avvio (facoltativa)\n\
  --db FILE    database SQLite (default: $ORBIT_DB o ~/.local/share/orbit/orbit.db)\n\
  --port N     porta locale su 127.0.0.1 (default: automatica)\n\
  --headless   non apre una finestra: stampa l'indirizzo da aprire\n";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut path = None;
    let mut db = None;
    let mut port = 0u16;
    let mut headless = false;
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{}", USAGE);
                return ExitCode::SUCCESS;
            }
            "--headless" => headless = true,
            "--db" => db = args.next().map(PathBuf::from),
            "--port" => port = args.next().and_then(|p| p.parse().ok()).unwrap_or(0),
            other if !other.starts_with('-') && path.is_none() => path = Some(PathBuf::from(other)),
            other => {
                eprintln!("opzione non riconosciuta: {}\n{}", other, USAGE);
                return ExitCode::FAILURE;
            }
        }
    }
    let db = db.unwrap_or_else(OrbitStore::default_path);
    let store = match OrbitStore::open(&db) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("orbit-app: {}: {}", db.display(), e);
            return ExitCode::FAILURE;
        }
    };
    match run(RunOptions {
        store,
        port,
        initial_scan: path,
        open_window: !headless,
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("orbit-app: {}", e);
            ExitCode::FAILURE
        }
    }
}
