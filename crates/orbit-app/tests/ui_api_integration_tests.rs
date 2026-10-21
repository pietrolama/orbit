//! Integration tests of the local API over real TCP: authorization, origin policy, limits, and
//! the main endpoints. Files live in temporary directories; `xdg-open` is replaced by a recorder.

use orbit_app::{AppServer, AppState, Opener};
use orbit_core::index::{index_scan, scan_config_for, IndexOptions};
use orbit_core::{scan_directory, CancelToken, OrbitStore};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;

struct Fixture {
    _data: TempDir,
    _db: TempDir,
    root: PathBuf,
    server: AppServer,
    opened: Arc<Mutex<Vec<PathBuf>>>,
    token: String,
}

fn write(p: &Path, b: &[u8]) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, b).unwrap();
}

fn fixture() -> Fixture {
    let data = TempDir::new().unwrap();
    let db = TempDir::new().unwrap();
    let root = std::fs::canonicalize(data.path()).unwrap();
    write(&root.join("lavoro/fattura_2026.pdf"), b"%PDF fattura");
    write(
        &root.join("archivio/fattura_2026_v2.pdf"),
        b"%PDF fattura v2",
    );
    write(
        &root.join("note/README.md"),
        b"Vedi [fattura](../lavoro/fattura_2026.pdf)",
    );
    write(&root.join("script.sh"), b"#!/bin/sh\necho ciao\n");
    std::fs::set_permissions(
        root.join("script.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let store = OrbitStore::open(db.path().join("orbit.db")).unwrap();
    let scan = scan_directory(&scan_config_for(&root).unwrap(), &CancelToken::new());
    index_scan(&store, &scan, None, &IndexOptions::default()).unwrap();
    let opened = Arc::new(Mutex::new(Vec::new()));
    let rec = opened.clone();
    let opener: Opener = Arc::new(move |p: &Path| {
        rec.lock().unwrap().push(p.to_path_buf());
        Ok(())
    });
    let state = AppState::with_opener(store, opener).unwrap();
    let server = AppServer::start(0, state).unwrap();
    let token = server.session_token().to_string();
    Fixture {
        _data: data,
        _db: db,
        root,
        server,
        opened,
        token,
    }
}

struct Resp {
    status: u16,
    body: Vec<u8>,
    headers: String,
}

impl Resp {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

fn raw(port: u16, request: &[u8]) -> Resp {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(request).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http response");
    let headers = String::from_utf8_lossy(&buf[..split]).into_owned();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    Resp {
        status,
        body: buf[split + 4..].to_vec(),
        headers,
    }
}

fn req(f: &Fixture, method: &str, path: &str, body: Option<Value>, extra: &[(&str, &str)]) -> Resp {
    let port = f.server.addr().port();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut r = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nContent-Length: {}\r\n",
        method,
        path,
        port,
        body.len()
    );
    let mut has_ct = false;
    let mut has_tok = false;
    for (k, v) in extra {
        has_ct |= k.eq_ignore_ascii_case("content-type");
        has_tok |= k.eq_ignore_ascii_case("x-orbit-token");
        if !v.is_empty() {
            r.push_str(&format!("{}: {}\r\n", k, v));
        }
    }
    if !has_tok {
        r.push_str(&format!("X-Orbit-Token: {}\r\n", f.token));
    }
    if method == "POST" && !has_ct {
        r.push_str("Content-Type: application/json\r\n");
    }
    r.push_str("\r\n");
    r.push_str(&body);
    raw(port, r.as_bytes())
}

fn get(f: &Fixture, path: &str) -> Resp {
    req(f, "GET", path, None, &[])
}

fn post(f: &Fixture, path: &str, body: Value) -> Resp {
    req(f, "POST", path, Some(body), &[])
}

fn entity_by_name(f: &Fixture, name: &str) -> i64 {
    let r = post(
        f,
        "/api/search",
        json!({ "query": format!("name:{}", name) }),
    );
    assert_eq!(r.status, 200);
    let v = r.json();
    v["search"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|x| x["name"] == name)
        .unwrap_or_else(|| panic!("{} not found: {}", name, v))["entity_id"]
        .as_i64()
        .unwrap()
}

// ---- Security --------------------------------------------------------------------------------

#[test]
fn requests_without_or_with_wrong_token_are_rejected() {
    let f = fixture();
    assert_eq!(
        req(&f, "GET", "/api/status", None, &[("X-Orbit-Token", "")]).status,
        401
    );
    assert_eq!(
        req(
            &f,
            "GET",
            "/api/status",
            None,
            &[("X-Orbit-Token", "deadbeef")]
        )
        .status,
        401
    );
    let wrong = "0".repeat(64);
    assert_eq!(
        req(
            &f,
            "POST",
            "/api/scan/start",
            Some(json!({"root_path": "/"})),
            &[("X-Orbit-Token", &wrong)]
        )
        .status,
        401
    );
    assert_eq!(
        req(
            &f,
            "POST",
            "/api/relations/create",
            Some(json!({"a": 1, "b": 2})),
            &[("X-Orbit-Token", "")]
        )
        .status,
        401
    );
    // The query-string token is accepted only for previews, never for other endpoints.
    let q = format!("/api/status?token={}", f.token);
    assert_eq!(
        req(&f, "GET", &q, None, &[("X-Orbit-Token", "")]).status,
        401
    );
    assert_eq!(get(&f, "/api/status").status, 200);
}

#[test]
fn launch_code_is_single_use() {
    let f = fixture();
    let code = f.server.launch_code().to_string();
    assert!(f.server.launch_url().contains(&format!("#launch={}", code)));
    let r = req(
        &f,
        "POST",
        "/api/session",
        Some(json!({ "launch_code": code })),
        &[("X-Orbit-Token", "")],
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["token"], f.token.as_str());
    let again = req(
        &f,
        "POST",
        "/api/session",
        Some(json!({ "launch_code": code })),
        &[("X-Orbit-Token", "")],
    );
    assert_eq!(again.status, 401);
}

#[test]
fn foreign_hosts_origins_and_cross_site_requests_are_rejected() {
    let f = fixture();
    let port = f.server.addr().port();
    let r = raw(
        port,
        format!(
            "GET /api/status HTTP/1.1\r\nHost: evil.example:{}\r\nX-Orbit-Token: {}\r\n\r\n",
            port, f.token
        )
        .as_bytes(),
    );
    assert_eq!(r.status, 403, "DNS rebinding");
    let r = raw(
        port,
        format!(
            "GET /api/status HTTP/1.1\r\nX-Orbit-Token: {}\r\n\r\n",
            f.token
        )
        .as_bytes(),
    );
    assert_eq!(r.status, 403, "missing Host");
    assert_eq!(
        req(
            &f,
            "GET",
            "/api/status",
            None,
            &[("Origin", "https://evil.example")]
        )
        .status,
        403
    );
    assert_eq!(
        req(
            &f,
            "GET",
            "/api/status",
            None,
            &[("Referer", "http://127.0.0.1:1/x")]
        )
        .status,
        403
    );
    assert_eq!(
        req(
            &f,
            "GET",
            "/api/status",
            None,
            &[("Sec-Fetch-Site", "cross-site")]
        )
        .status,
        403
    );
    let own = format!("http://127.0.0.1:{}", port);
    assert_eq!(
        req(&f, "GET", "/api/status", None, &[("Origin", own.as_str())]).status,
        200
    );
    // Simple cross-site form posts cannot carry JSON content type.
    assert_eq!(
        req(
            &f,
            "POST",
            "/api/scan/cancel",
            Some(json!({})),
            &[("Content-Type", "text/plain")]
        )
        .status,
        415
    );
    assert_eq!(req(&f, "DELETE", "/api/status", None, &[]).status, 405);
}

#[test]
fn oversized_and_malformed_requests_are_rejected() {
    let f = fixture();
    let port = f.server.addr().port();
    let r = raw(port, format!("POST /api/search HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-Orbit-Token: {}\r\nContent-Type: application/json\r\nContent-Length: 99999999\r\n\r\n", port, f.token).as_bytes());
    assert_eq!(r.status, 413);
    let long = "a".repeat(20_000);
    let r = raw(
        port,
        format!(
            "GET /api/status HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-Long: {}\r\n\r\n",
            port, long
        )
        .as_bytes(),
    );
    assert_eq!(r.status, 431);
    assert_eq!(post(&f, "/api/search", json!({"wrong": 1})).status, 400);
    let r = req(&f, "POST", "/api/search", None, &[]);
    assert_eq!(r.status, 400);
    assert_eq!(get(&f, "/api/graph/region?x0=a").status, 400);
}

#[test]
fn static_ui_is_served_with_strict_headers() {
    let f = fixture();
    let r = req(&f, "GET", "/", None, &[("X-Orbit-Token", "")]);
    assert_eq!(r.status, 200);
    assert!(r
        .headers
        .contains("Content-Security-Policy: default-src 'none'; script-src 'self'"));
    assert!(r.headers.contains("X-Frame-Options: DENY"));
    assert!(
        !String::from_utf8_lossy(&r.body).contains(&f.token),
        "the token is never embedded in pages"
    );
    assert_eq!(
        req(&f, "GET", "/app.js", None, &[("X-Orbit-Token", "")]).status,
        200
    );
    assert_eq!(
        req(&f, "GET", "/../etc/passwd", None, &[("X-Orbit-Token", "")]).status,
        404
    );
}

#[test]
fn open_file_never_executes_and_verifies_identity() {
    let f = fixture();
    let script = entity_by_name(&f, "script.sh");
    let r = post(
        &f,
        "/api/open-file",
        json!({ "entity_id": script, "mode": "open" }),
    );
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["opened"], false);
    assert!(
        f.opened.lock().unwrap().is_empty(),
        "executables are never handed to xdg-open"
    );
    let r = post(
        &f,
        "/api/open-file",
        json!({ "entity_id": script, "mode": "reveal" }),
    );
    assert_eq!(r.json()["opened"], true);
    assert_eq!(f.opened.lock().unwrap().last().unwrap(), &f.root);

    let pdf = entity_by_name(&f, "fattura_2026.pdf");
    assert_eq!(
        post(&f, "/api/open-file", json!({ "entity_id": pdf })).json()["opened"],
        true
    );
    // Replace the file behind ORBIT's back: the path now names a different object.
    let p = f.root.join("lavoro/fattura_2026.pdf");
    std::fs::remove_file(&p).unwrap();
    write(&p, b"altro");
    assert_eq!(
        post(&f, "/api/open-file", json!({ "entity_id": pdf })).status,
        409
    );
    let md = entity_by_name(&f, "README.md");
    assert_eq!(
        post(
            &f,
            "/api/open-file",
            json!({ "entity_id": md, "mode": "rm -rf" })
        )
        .status,
        400
    );
    assert_eq!(
        post(&f, "/api/open-file", json!({ "entity_id": 999_999 })).status,
        404
    );
}

// ---- Functional endpoints --------------------------------------------------------------------

#[test]
fn graph_views_entity_detail_and_preview() {
    let f = fixture();
    let ov = get(&f, "/api/graph/overview").json();
    assert!(ov["total_nodes"].as_u64().unwrap() >= 8);
    let b = &ov["bounds"];
    let q = format!(
        "/api/graph/region?x0={}&y0={}&x1={}&y1={}&scale=50&min_px=0",
        b[0], b[1], b[2], b[3]
    );
    let region = get(&f, &q).json();
    assert!(region["nodes"].as_array().unwrap().len() >= 8);
    let pdf = entity_by_name(&f, "fattura_2026.pdf");
    let d = get(&f, &format!("/api/graph/entity?id={}", pdf)).json();
    let kinds: Vec<&str> = d["relations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"name_similarity"), "{:?}", kinds);
    assert!(kinds.contains(&"explicit_reference"), "{:?}", kinds);
    assert!(!d["relations"][0]["reasons"].as_array().unwrap().is_empty());
    assert_eq!(d["observations"].as_array().unwrap().len(), 1);

    let md = entity_by_name(&f, "README.md");
    let p = req(
        &f,
        "GET",
        &format!("/api/preview?id={}&token={}", md, f.token),
        None,
        &[("X-Orbit-Token", "")],
    );
    assert_eq!(p.status, 200);
    assert!(p.json()["text"].as_str().unwrap().contains("fattura"));
    assert_eq!(get(&f, &format!("/api/preview?id={}", pdf)).status, 415);
}

#[test]
fn search_returns_neighbors_and_persistent_coordinates_without_changing_them() {
    let f = fixture();
    let before = get(&f, "/api/graph/overview").json()["bounds"].clone();
    let r = post(&f, "/api/search", json!({ "query": "fattura" })).json();
    let results = r["search"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert!(!results[0]["relation_neighbors"]
        .as_array()
        .unwrap()
        .is_empty());
    let nodes = r["nodes"].as_array().unwrap();
    assert!(nodes
        .iter()
        .all(|n| n["x"].is_number() && n["y"].is_number()));
    assert_eq!(
        get(&f, "/api/graph/overview").json()["bounds"],
        before,
        "search is read-only"
    );
    let none = post(&f, "/api/search", json!({ "query": "zzzz-niente" })).json();
    assert_eq!(none["search"]["total_matches"], 0);
}

#[test]
fn manual_relations_positions_pins_and_constellations() {
    let f = fixture();
    let a = entity_by_name(&f, "fattura_2026.pdf");
    let b = entity_by_name(&f, "script.sh");
    let e = post(
        &f,
        "/api/relations/create",
        json!({ "a": a, "b": b, "note": "stesso cliente" }),
    )
    .json();
    let edge = e["id"].as_i64().unwrap();
    assert_eq!(e["origin"], "manual");
    let ex = post(&f, "/api/relations/explain", json!({ "a": a, "b": b })).json();
    assert!(ex["combined_score"].as_f64().unwrap() > 0.0);
    assert_eq!(
        post(&f, "/api/relations/delete", json!({ "edge_id": edge })).json()["state"],
        "deleted"
    );
    assert_eq!(
        post(&f, "/api/relations/restore", json!({ "edge_id": edge })).json()["state"],
        "active"
    );
    assert_eq!(
        post(&f, "/api/relations/create", json!({ "a": a, "b": a })).status,
        400
    );

    // Drag (manual position) + pin; then a refresh must not move it.
    let r = post(
        &f,
        "/api/positions/save",
        json!({ "positions": [{ "entity_id": a, "x": 4242.0, "y": -17.5 }] }),
    );
    assert_eq!(r.status, 200);
    assert_eq!(
        post(
            &f,
            "/api/positions/pin",
            json!({ "entity_ids": [a], "pinned": true })
        )
        .status,
        200
    );
    assert_eq!(post(&f, "/api/relations/refresh", json!({})).status, 200);
    assert_eq!(
        post(&f, "/api/layout/reset", json!({ "seed": 5 })).status,
        200
    );
    let d = get(&f, &format!("/api/graph/entity?id={}", a)).json();
    assert_eq!(d["position"]["x"], 4242.0);
    assert_eq!(d["position"]["pinned"], true);
    assert_eq!(d["position"]["source"], "manual");
    assert_eq!(
        post(
            &f,
            "/api/positions/save",
            json!({ "positions": [{ "entity_id": 999_999, "x": 1.0, "y": 1.0 }] })
        )
        .status,
        404
    );

    let c = post(
        &f,
        "/api/constellations/create",
        json!({ "name": "Cliente Rossi", "color": "#ff8800", "members": [a, b] }),
    )
    .json();
    let cid = c["id"].as_i64().unwrap();
    assert_eq!(c["members"].as_array().unwrap().len(), 2);
    let ov = get(&f, "/api/graph/overview").json();
    assert_eq!(
        ov["constellations"][0]["member_positions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let u = post(
        &f,
        "/api/constellations/update",
        json!({ "id": cid, "remove": [b], "name": "Rossi" }),
    )
    .json();
    assert_eq!(u["members"].as_array().unwrap().len(), 1);
    assert_eq!(
        post(
            &f,
            "/api/constellations/create",
            json!({ "name": "x", "color": "red" })
        )
        .status,
        400
    );
}

#[test]
fn scan_job_history_and_recover() {
    let f = fixture();
    write(&f.root.join("nuovo/appunti.txt"), b"nuovo");
    let r = post(
        &f,
        "/api/scan/start",
        json!({ "root_path": f.root.to_string_lossy() }),
    );
    assert_eq!(r.status, 200);
    assert_eq!(
        post(&f, "/api/scan/start", json!({ "root_path": "relativo" })).status,
        400
    );
    let started = Instant::now();
    loop {
        let s = get(&f, "/api/status").json();
        if s["job"].is_null() {
            assert!(s["last_error"].is_null(), "{}", s);
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "scan did not finish"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let snaps = get(&f, "/api/history/snapshots").json();
    let snaps = snaps.as_array().unwrap();
    assert_eq!(snaps.len(), 2);
    let d = post(
        &f,
        "/api/history/diff",
        json!({ "base": snaps[1]["id"], "target": snaps[0]["id"] }),
    )
    .json();
    assert_eq!(d["totals"]["appeared"], 2, "{}", d["appeared"]);
    let rec = get(&f, "/api/recover").json();
    assert!(rec["notes"][0].as_str().unwrap().contains("non elimina"));
}
