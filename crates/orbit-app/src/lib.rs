//! # `orbit-app`
//!
//! Local HTTP/JSON API and static Canvas UI for ORBIT. The transport is inherited from DiskGarden
//! and hardened (see `SECURITY.md`):
//!
//! 1. loopback-only binding (`127.0.0.1`), which by itself is **not** treated as sufficient;
//! 2. single-use *launch code* (in the URL fragment) exchanged once for a 256-bit *session token*
//!    that never appears in process arguments, URLs or logs;
//! 3. every `/api/*` request (except the exchange) must carry `X-Orbit-Token`, compared in constant
//!    time; only `GET /api/preview` also accepts `?token=` (for `<img>` elements);
//! 4. strict `Host` validation (DNS rebinding), `Origin`/`Referer`/`Sec-Fetch-Site` validation
//!    (cross-site requests), `Content-Type: application/json` required on `POST`;
//! 5. limits on header size, body size (1 MiB), concurrent connections and socket timeouts;
//! 6. files are opened through `xdg-open` with an argument vector (no shell), only for paths
//!    resolved from the database and re-verified on disk; executables are never opened.

use orbit_core::analyzer::space_report;
use orbit_core::fingerprint::FingerprintBudget;
use orbit_core::graph::{
    preview_kind, Graph, PreviewKind, RegionQuery, PREVIEW_MAX_IMAGE_BYTES, PREVIEW_TEXT_BYTES,
};
use orbit_core::history::diff_snapshots;
use orbit_core::index::{index_scan, refresh, relayout, IndexOptions, IndexReport};
use orbit_core::layout::{NodePosition, PositionSource};
use orbit_core::relations::{combined_score, RelationConfig};
use orbit_core::search::{search, SearchOptions};
use orbit_core::store::MAIN_CANVAS;
use orbit_core::{
    raw_bytes_to_path_buf, CancelToken, DbError, EdgeOrigin, EdgeState, EntityId, EntryType,
    OrbitStore, RelationKind, ScanConfig, ScanProgress, ScanStatus,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Version of the JSON API contract (`docs/ARCHITECTURE.md#api`).
pub const API_VERSION: u32 = 1;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_HEADER_LINE: usize = 8 * 1024;
const MAX_HEADERS: usize = 64;
const MAX_CONNECTIONS: usize = 64;
const MAX_POSITIONS_PER_SAVE: usize = 10_000;

const UI_INDEX_HTML: &str = include_str!("../ui/index.html");
const UI_APP_JS: &str = include_str!("../ui/app.js");
const UI_RENDERER_JS: &str = include_str!("../ui/renderer.js");
const UI_STYLES_CSS: &str = include_str!("../ui/styles.css");

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Opens a path with the desktop's default handler. Injectable so tests never spawn processes.
pub type Opener = Arc<dyn Fn(&Path) -> std::io::Result<()> + Send + Sync>;

fn xdg_open(path: &Path) -> std::io::Result<()> {
    // Argument vector, no shell: arbitrary bytes in `path` cannot be interpreted as commands.
    let mut child = Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

// ================================================================================================
// Application state
// ================================================================================================

#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    pub job_id: u64,
    pub root_path: String,
    /// `scanning` or `indexing`.
    pub phase: String,
    pub progress: Option<ScanProgress>,
}

struct Inner {
    graph: Arc<Graph>,
    graph_version: u64,
    job: Option<(JobStatus, CancelToken)>,
    next_job: u64,
    last_report: Option<IndexReport>,
    last_error: Option<String>,
}

/// Shared state between request threads and background jobs.
#[derive(Clone)]
pub struct AppState {
    store: OrbitStore,
    canvas_id: i64,
    inner: Arc<Mutex<Inner>>,
    opener: Opener,
}

impl AppState {
    pub fn new(store: OrbitStore) -> Result<Self, DbError> {
        Self::with_opener(store, Arc::new(xdg_open))
    }

    pub fn with_opener(store: OrbitStore, opener: Opener) -> Result<Self, DbError> {
        let canvas_id = store.canvas_id(MAIN_CANVAS)?;
        let graph = Graph::load(&store, canvas_id, RelationConfig::default())?;
        Ok(Self {
            store,
            canvas_id,
            inner: Arc::new(Mutex::new(Inner {
                graph: Arc::new(graph),
                graph_version: 1,
                job: None,
                next_job: 1,
                last_report: None,
                last_error: None,
            })),
            opener,
        })
    }

    pub fn store(&self) -> &OrbitStore {
        &self.store
    }

    pub fn graph(&self) -> Arc<Graph> {
        self.inner.lock().unwrap().graph.clone()
    }

    pub fn reload_graph(&self) -> Result<(), DbError> {
        let g = Graph::load(&self.store, self.canvas_id, RelationConfig::default())?;
        let mut inner = self.inner.lock().unwrap();
        inner.graph = Arc::new(g);
        inner.graph_version += 1;
        Ok(())
    }

    fn update_graph(&self, f: impl FnOnce(&mut Graph)) {
        let mut inner = self.inner.lock().unwrap();
        // Copy-on-write: clones only while a request thread still holds the previous graph.
        f(Arc::make_mut(&mut inner.graph));
        inner.graph_version += 1;
    }

    pub fn is_busy(&self) -> bool {
        self.inner.lock().unwrap().job.is_some()
    }

    /// Starts a background scan + indexing job. Refused while another job runs.
    pub fn start_scan(&self, config: ScanConfig, opts: IndexOptions) -> Result<u64, String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.job.is_some() {
            return Err("un'altra scansione è in corso".to_string());
        }
        let job_id = inner.next_job;
        inner.next_job += 1;
        let cancel = CancelToken::new();
        inner.job = Some((
            JobStatus {
                job_id,
                root_path: config.root_path.display().to_string(),
                phase: "scanning".into(),
                progress: None,
            },
            cancel.clone(),
        ));
        inner.last_error = None;
        drop(inner);

        let state = self.clone();
        thread::spawn(move || {
            let st = state.clone();
            let scan = orbit_core::scan_directory_with_progress(&config, &cancel, |p| {
                if let Some((job, _)) = st.inner.lock().unwrap().job.as_mut() {
                    job.progress = Some(p.clone());
                }
            });
            let outcome: Result<Option<IndexReport>, String> = match scan.status {
                ScanStatus::Cancelled => Ok(None),
                ScanStatus::Failed => Err(format!(
                    "scansione fallita: {}",
                    scan.root_error_message.clone().unwrap_or_default()
                )),
                _ => {
                    if let Some((job, _)) = state.inner.lock().unwrap().job.as_mut() {
                        job.phase = "indexing".into();
                    }
                    index_scan(&state.store, &scan, None, &opts)
                        .map(Some)
                        .map_err(|e| e.to_string())
                }
            };
            let reload = state.reload_graph();
            let mut inner = state.inner.lock().unwrap();
            inner.job = None;
            match outcome {
                Ok(Some(r)) => inner.last_report = Some(r),
                Ok(None) => {
                    inner.last_error =
                        Some("scansione annullata: nessuno snapshot registrato".into())
                }
                Err(e) => inner.last_error = Some(e),
            }
            if let Err(e) = reload {
                inner.last_error = Some(e.to_string());
            }
        });
        Ok(job_id)
    }

    pub fn cancel_scan(&self) -> bool {
        match self.inner.lock().unwrap().job.as_ref() {
            Some((_, token)) => {
                token.cancel();
                true
            }
            None => false,
        }
    }

    /// Recomputes relations and layout without scanning (synchronous).
    pub fn refresh_relations(&self, verify_full: bool) -> Result<IndexReport, String> {
        if self.is_busy() {
            return Err("una scansione è in corso".into());
        }
        let opts = IndexOptions {
            fingerprint: FingerprintBudget {
                verify_full,
                ..FingerprintBudget::default()
            },
            ..IndexOptions::default()
        };
        let r = refresh(&self.store, &opts).map_err(|e| e.to_string())?;
        self.reload_graph().map_err(|e| e.to_string())?;
        self.inner.lock().unwrap().last_report = Some(r.clone());
        Ok(r)
    }
}

// ================================================================================================
// Server
// ================================================================================================

/// Embedded local HTTP server.
pub struct AppServer {
    addr: SocketAddr,
    state: AppState,
    launch_code: String,
    auth: Arc<AuthState>,
}

struct AuthState {
    session_token: String,
    launch_code: Mutex<Option<String>>,
}

impl AppServer {
    /// Binds to `127.0.0.1:port` (`0` = ephemeral) and serves requests on worker threads.
    pub fn start(port: u16, state: AppState) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let addr = listener.local_addr()?;
        let launch_code = random_hex(32)?;
        let auth = Arc::new(AuthState {
            session_token: random_hex(32)?,
            launch_code: Mutex::new(Some(launch_code.clone())),
        });
        let st = state.clone();
        let au = auth.clone();
        let active = Arc::new(AtomicUsize::new(0));
        let server_port = addr.port();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    let mut s = stream;
                    let _ = write_json(
                        &mut s,
                        503,
                        &serde_json::json!({"error": "troppe connessioni simultanee"}),
                    );
                    continue;
                }
                active.fetch_add(1, Ordering::SeqCst);
                let st = st.clone();
                let au = au.clone();
                let active = active.clone();
                thread::spawn(move || {
                    let _ = handle_connection(stream, &st, &au, server_port);
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        Ok(Self {
            addr,
            state,
            launch_code,
            auth,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// URL that opens the UI. The launch code travels in the fragment (never sent to the server
    /// as part of the request line, never logged) and is valid once.
    pub fn launch_url(&self) -> String {
        format!(
            "http://127.0.0.1:{}/#launch={}",
            self.addr.port(),
            self.launch_code
        )
    }

    /// Session token (for tests and embedding hosts that can set headers).
    pub fn session_token(&self) -> &str {
        &self.auth.session_token
    }

    pub fn launch_code(&self) -> &str {
        &self.launch_code
    }

    pub fn state(&self) -> &AppState {
        &self.state
    }
}

fn random_hex(n: usize) -> std::io::Result<String> {
    let mut bytes = vec![0u8; n];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut hex = String::with_capacity(n * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut hex, "{:02x}", b);
    }
    Ok(hex)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn is_allowed_host(host_header: &str, expected_port: u16) -> bool {
    let trimmed = host_header.trim();
    let (host, port_opt) = match trimmed.rsplit_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (trimmed, None),
    };
    let host_ok = host.eq_ignore_ascii_case("127.0.0.1") || host.eq_ignore_ascii_case("localhost");
    let port_ok = match port_opt {
        Some(p) => p.parse::<u16>().ok() == Some(expected_port),
        None => expected_port == 80,
    };
    host_ok && port_ok
}

fn is_allowed_origin(val: &str, expected_port: u16) -> bool {
    let trimmed = val.trim();
    [
        format!("http://127.0.0.1:{}", expected_port),
        format!("http://localhost:{}", expected_port),
    ]
    .iter()
    .any(|prefix| {
        trimmed
            .strip_prefix(prefix.as_str())
            .map(|rest| rest.is_empty() || rest.starts_with('/'))
            .unwrap_or(false)
    })
}

/// A parsed HTTP request.
struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    body: Vec<u8>,
    headers: HashMap<String, String>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    fn json<T: for<'de> Deserialize<'de>>(&self) -> Result<T, Response> {
        serde_json::from_slice(&self.body)
            .map_err(|e| Response::error(400, &format!("richiesta non valida: {}", e)))
    }

    fn query_f64(&self, key: &str) -> Option<f64> {
        self.query
            .get(key)
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
    }

    fn query_i64(&self, key: &str) -> Option<i64> {
        self.query.get(key).and_then(|v| v.parse::<i64>().ok())
    }
}

/// An HTTP response.
struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn json<T: Serialize>(status: u16, v: &T) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn ok<T: Serialize>(v: &T) -> Self {
        Self::json(200, v)
    }

    fn error(status: u16, msg: &str) -> Self {
        Self::json(status, &serde_json::json!({ "error": msg }))
    }

    fn db(e: DbError) -> Self {
        match e {
            DbError::NotFound(m) => Self::error(404, &format!("non trovato: {}", m)),
            DbError::Invalid(m) => Self::error(400, &m),
            other => Self::error(500, &other.to_string()),
        }
    }

    fn raw(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type,
            body,
        }
    }
}

fn read_request(stream: &mut TcpStream) -> Result<Request, Response> {
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|_| Response::error(500, "socket"))?,
    );
    let mut read_line = |limit: usize| -> Result<String, Response> {
        let mut buf = Vec::new();
        let n = (&mut reader)
            .take(limit as u64 + 1)
            .read_until(b'\n', &mut buf)
            .map_err(|_| Response::error(400, "richiesta incompleta"))?;
        if n > limit {
            return Err(Response::error(431, "intestazione troppo lunga"));
        }
        Ok(String::from_utf8_lossy(&buf).into_owned())
    };
    let request_line = read_line(MAX_HEADER_LINE)?;
    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return Err(Response::error(400, "richiesta non valida"));
    };
    let (method, target) = (method.to_string(), target.to_string());
    let mut headers = HashMap::new();
    for _ in 0..=MAX_HEADERS {
        let line = read_line(MAX_HEADER_LINE)?;
        if line == "\r\n" || line == "\n" || line.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(Response::error(431, "troppe intestazioni"));
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let content_length = match headers.get("content-length") {
        Some(v) => v
            .parse::<usize>()
            .map_err(|_| Response::error(400, "Content-Length non valido"))?,
        None => 0,
    };
    if headers.contains_key("transfer-encoding") {
        return Err(Response::error(501, "Transfer-Encoding non supportato"));
    }
    if content_length > MAX_BODY_BYTES {
        return Err(Response::error(413, "corpo della richiesta troppo grande"));
    }
    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|_| Response::error(400, "corpo della richiesta incompleto"))?;
    let (path, query_str) = target.split_once('?').unwrap_or((&target, ""));
    Ok(Request {
        method,
        path: path.to_string(),
        query: parse_query(query_str),
        body,
        headers,
    })
}

fn handle_connection(
    mut stream: TcpStream,
    state: &AppState,
    auth: &AuthState,
    port: u16,
) -> std::io::Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(30)));
    let resp = match read_request(&mut stream) {
        Ok(req) => route(&req, state, auth, port),
        Err(r) => r,
    };
    write_response(&mut stream, &resp)
}

fn route(req: &Request, state: &AppState, auth: &AuthState, port: u16) -> Response {
    // 1. Host (DNS rebinding).
    match req.header("host") {
        Some(h) if is_allowed_host(h, port) => {}
        Some(_) => return Response::error(403, "Host non autorizzato (protezione DNS rebinding)"),
        None => return Response::error(403, "intestazione Host mancante"),
    }
    // 2. Cross-site requests.
    for h in ["origin", "referer"] {
        if let Some(v) = req.header(h) {
            if !is_allowed_origin(v, port) {
                return Response::error(403, "origine non autorizzata (protezione cross-site)");
            }
        }
    }
    if req.header("sec-fetch-site") == Some("cross-site") {
        return Response::error(403, "richiesta cross-site rifiutata");
    }

    // 3. Static UI (public code, no secrets).
    if req.method == "GET" {
        match req.path.as_str() {
            "/" | "/index.html" => {
                return Response::raw(
                    200,
                    "text/html; charset=utf-8",
                    UI_INDEX_HTML.as_bytes().to_vec(),
                )
            }
            "/app.js" => {
                return Response::raw(
                    200,
                    "text/javascript; charset=utf-8",
                    UI_APP_JS.as_bytes().to_vec(),
                )
            }
            "/renderer.js" => {
                return Response::raw(
                    200,
                    "text/javascript; charset=utf-8",
                    UI_RENDERER_JS.as_bytes().to_vec(),
                )
            }
            "/styles.css" => {
                return Response::raw(
                    200,
                    "text/css; charset=utf-8",
                    UI_STYLES_CSS.as_bytes().to_vec(),
                )
            }
            _ => {}
        }
    }
    if !req.path.starts_with("/api/") {
        return Response::error(404, "risorsa non trovata");
    }
    if req.method == "POST" {
        let ct = req.header("content-type").unwrap_or("");
        if !ct.to_ascii_lowercase().starts_with("application/json") {
            return Response::error(415, "Content-Type application/json obbligatorio");
        }
    } else if req.method != "GET" {
        return Response::error(405, "metodo non consentito");
    }

    // 4. Launch-code exchange (single use).
    if req.method == "POST" && req.path == "/api/session" {
        #[derive(Deserialize)]
        struct S {
            launch_code: String,
        }
        let s: S = match req.json() {
            Ok(s) => s,
            Err(r) => return r,
        };
        let mut code = auth.launch_code.lock().unwrap();
        let valid = code
            .as_deref()
            .map(|c| constant_time_eq(c.as_bytes(), s.launch_code.as_bytes()))
            .unwrap_or(false);
        if !valid {
            return Response::error(401, "codice di avvio non valido o già usato");
        }
        *code = None;
        return Response::ok(
            &serde_json::json!({ "token": auth.session_token, "api_version": API_VERSION }),
        );
    }

    // 5. Session token.
    let supplied = req.header("x-orbit-token").or_else(|| {
        (req.method == "GET" && req.path == "/api/preview")
            .then(|| req.query.get("token").map(String::as_str))
            .flatten()
    });
    let ok = supplied
        .map(|t| constant_time_eq(t.as_bytes(), auth.session_token.as_bytes()))
        .unwrap_or(false);
    if !ok {
        return Response::error(401, "token di sessione mancante o non valido");
    }

    api(req, state)
}

// ================================================================================================
// API handlers
// ================================================================================================

#[derive(Deserialize)]
struct ScanStartReq {
    root_path: String,
    #[serde(default)]
    follow_symlinks: bool,
    #[serde(default)]
    cross_mounts: bool,
    #[serde(default)]
    verify_duplicates: bool,
}

#[derive(Deserialize)]
struct SearchReq {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct DiffReq {
    base: i64,
    target: i64,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct PosItem {
    entity_id: EntityId,
    x: f64,
    y: f64,
}

#[derive(Deserialize)]
struct PosSaveReq {
    positions: Vec<PosItem>,
    /// Apply the same translation to all unlocked descendants of each moved directory.
    #[serde(default)]
    with_descendants: bool,
    #[serde(default)]
    pin: bool,
}

#[derive(Deserialize)]
struct PinReq {
    entity_ids: Vec<EntityId>,
    pinned: bool,
}

#[derive(Deserialize)]
struct IdsReq {
    entity_ids: Vec<EntityId>,
}

#[derive(Deserialize)]
struct ConstCreateReq {
    name: String,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    members: Vec<EntityId>,
}

#[derive(Deserialize)]
struct ConstUpdateReq {
    id: i64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    add: Vec<EntityId>,
    #[serde(default)]
    remove: Vec<EntityId>,
    #[serde(default)]
    deleted: Option<bool>,
}

#[derive(Deserialize)]
struct RelCreateReq {
    a: EntityId,
    b: EntityId,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Deserialize)]
struct EdgeIdReq {
    edge_id: i64,
}

#[derive(Deserialize)]
struct PairReq {
    a: EntityId,
    b: EntityId,
}

#[derive(Deserialize)]
struct OpenReq {
    entity_id: EntityId,
    /// `open` (default application) or `reveal` (containing folder).
    #[serde(default = "default_open_mode")]
    mode: String,
}

fn default_open_mode() -> String {
    "open".into()
}

fn api(req: &Request, state: &AppState) -> Response {
    let store = state.store();
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/api/status") => {
            let inner = state.inner.lock().unwrap();
            let s = store.status();
            Response::ok(&serde_json::json!({
                "api_version": API_VERSION,
                "graph_version": inner.graph_version,
                "job": inner.job.as_ref().map(|(j, _)| j),
                "last_report": inner.last_report,
                "last_error": inner.last_error,
                "store": s.ok(),
                "home": std::env::var("HOME").ok(),
            }))
        }
        ("POST", "/api/scan/start") => {
            let r: ScanStartReq = match req.json() {
                Ok(r) => r,
                Err(e) => return e,
            };
            let path = PathBuf::from(r.root_path.trim());
            if !path.is_absolute() {
                return Response::error(400, "il percorso deve essere assoluto");
            }
            let canonical = match fs::canonicalize(&path) {
                Ok(p) if p.is_dir() => p,
                Ok(_) => return Response::error(400, "il percorso non è una directory"),
                Err(e) => return Response::error(400, &format!("percorso non accessibile: {}", e)),
            };
            let mut cfg = ScanConfig::new(canonical);
            cfg.follow_symlinks = r.follow_symlinks;
            cfg.cross_mounts = r.cross_mounts;
            cfg.progress_interval_ms = 100;
            let opts = IndexOptions {
                fingerprint: FingerprintBudget {
                    verify_full: r.verify_duplicates,
                    ..FingerprintBudget::default()
                },
                ..IndexOptions::default()
            };
            match state.start_scan(cfg, opts) {
                Ok(id) => Response::ok(&serde_json::json!({ "started": true, "job_id": id })),
                Err(e) => Response::error(409, &e),
            }
        }
        ("POST", "/api/scan/cancel") => {
            Response::ok(&serde_json::json!({ "cancelled": state.cancel_scan() }))
        }
        ("POST", "/api/relations/refresh") => {
            #[derive(Deserialize)]
            struct R {
                #[serde(default)]
                verify_duplicates: bool,
            }
            let r: R = match req.json() {
                Ok(r) => r,
                Err(e) => return e,
            };
            match state.refresh_relations(r.verify_duplicates) {
                Ok(rep) => Response::ok(&rep),
                Err(e) => Response::error(409, &e),
            }
        }
        ("GET", "/api/graph/overview") => {
            let g = state.graph();
            let mc = req.query_i64("max_clusters").unwrap_or(300).clamp(1, 2000) as usize;
            let mf = req.query_i64("max_flows").unwrap_or(200).clamp(0, 5000) as usize;
            Response::ok(&g.overview(mc, mf))
        }
        ("GET", "/api/graph/region") => {
            let (Some(x0), Some(y0), Some(x1), Some(y1), Some(scale)) = (
                req.query_f64("x0"),
                req.query_f64("y0"),
                req.query_f64("x1"),
                req.query_f64("y1"),
                req.query_f64("scale"),
            ) else {
                return Response::error(400, "parametri x0, y0, x1, y1, scale obbligatori");
            };
            let kinds = req.query.get("kinds").filter(|k| !k.is_empty()).map(|k| {
                k.split(',')
                    .filter_map(RelationKind::from_db)
                    .collect::<Vec<_>>()
            });
            let q = RegionQuery {
                x0,
                y0,
                x1,
                y1,
                scale,
                min_px: req.query_f64("min_px").unwrap_or(0.6),
                max_nodes: req.query_i64("max_nodes").unwrap_or(2500).clamp(1, 20_000) as usize,
                max_edges: req.query_i64("max_edges").unwrap_or(1200).clamp(0, 20_000) as usize,
                min_strength: req.query_f64("min_strength").unwrap_or(0.0),
                kinds,
                focus: req.query_i64("focus"),
            };
            Response::ok(&state.graph().region(&q))
        }
        ("GET", "/api/graph/entity") => {
            let Some(id) = req.query_i64("id") else {
                return Response::error(400, "parametro id obbligatorio");
            };
            match state.graph().entity_detail(store, id) {
                Ok(d) => Response::ok(&d),
                Err(e) => Response::db(e),
            }
        }
        ("POST", "/api/search") => {
            let r: SearchReq = match req.json() {
                Ok(r) => r,
                Err(e) => return e,
            };
            if r.query.len() > 2000 {
                return Response::error(400, "query troppo lunga");
            }
            let opts = SearchOptions {
                limit: r.limit.unwrap_or(100).clamp(1, 500),
                ..SearchOptions::default()
            };
            let g = state.graph();
            let res = search(&g, &r.query, &opts, now_secs());
            // Render data (persistent coordinates) for results and their neighbours, so the UI can
            // animate them and restore them exactly.
            let mut ids: Vec<EntityId> = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for x in &res.results {
                for id in std::iter::once(&x.entity_id).chain(x.relation_neighbors.iter()) {
                    if seen.insert(*id) {
                        ids.push(*id);
                    }
                }
            }
            let nodes = g.node_views(&ids);
            Response::ok(&serde_json::json!({ "search": res, "nodes": nodes }))
        }
        ("GET", "/api/history/snapshots") => match store.list_snapshots() {
            Ok(s) => Response::ok(&s),
            Err(e) => Response::db(e),
        },
        ("POST", "/api/history/diff") => {
            let r: DiffReq = match req.json() {
                Ok(r) => r,
                Err(e) => return e,
            };
            match diff_snapshots(
                store,
                r.base,
                r.target,
                r.limit.unwrap_or(200).clamp(1, 5000),
            ) {
                Ok(d) => Response::ok(&d),
                Err(e) => Response::db(e),
            }
        }
        ("POST", "/api/positions/save") => save_positions(req, state),
        ("POST", "/api/positions/pin") => {
            let r: PinReq = match req.json() {
                Ok(r) => r,
                Err(e) => return e,
            };
            match store.set_pinned(state.canvas_id, &r.entity_ids, r.pinned) {
                Ok(n) => {
                    state.update_graph(|g| g.set_pinned_local(&r.entity_ids, r.pinned));
                    Response::ok(&serde_json::json!({ "updated": n }))
                }
                Err(e) => Response::db(e),
            }
        }
        ("POST", "/api/positions/release") => {
            let r: IdsReq = match req.json() {
                Ok(r) => r,
                Err(e) => return e,
            };
            match store
                .release_positions(state.canvas_id, &r.entity_ids)
                .and_then(|n| state.reload_graph().map(|_| n))
            {
                Ok(n) => Response::ok(&serde_json::json!({ "updated": n })),
                Err(e) => Response::db(e),
            }
        }
        ("POST", "/api/layout/reset") => {
            #[derive(Deserialize)]
            struct R {
                #[serde(default)]
                seed: Option<u64>,
            }
            let r: R = match req.json() {
                Ok(r) => r,
                Err(e) => return e,
            };
            if state.is_busy() {
                return Response::error(409, "una scansione è in corso");
            }
            match relayout(store, r.seed).and_then(|rep| state.reload_graph().map(|_| rep)) {
                Ok(rep) => Response::ok(&rep),
                Err(e) => Response::db(e),
            }
        }
        ("GET", "/api/constellations") => match store.list_constellations(true) {
            Ok(c) => Response::ok(&c),
            Err(e) => Response::db(e),
        },
        ("POST", "/api/constellations/create") => {
            let r: ConstCreateReq = match req.json() {
                Ok(r) => r,
                Err(e) => return e,
            };
            // The constellation's own spatial entity sits at the centroid of its members.
            let g = state.graph();
            let pts: Vec<(f64, f64)> = r
                .members
                .iter()
                .filter_map(|m| g.position(*m))
                .map(|p| (p.x, p.y))
                .collect();
            let centroid = (!pts.is_empty()).then(|| {
                let n = pts.len() as f64;
                (
                    pts.iter().map(|p| p.0).sum::<f64>() / n,
                    pts.iter().map(|p| p.1).sum::<f64>() / n,
                )
            });
            match store
                .create_constellation(&r.name, r.color.as_deref(), &r.members, centroid)
                .and_then(|c| state.reload_graph().map(|_| c))
            {
                Ok(c) => Response::ok(&c),
                Err(e) => Response::db(e),
            }
        }
        ("POST", "/api/constellations/update") => {
            let r: ConstUpdateReq = match req.json() {
                Ok(r) => r,
                Err(e) => return e,
            };
            match store
                .update_constellation(
                    r.id,
                    r.name.as_deref(),
                    r.color.as_deref(),
                    &r.add,
                    &r.remove,
                    r.deleted,
                )
                .and_then(|c| state.reload_graph().map(|_| c))
            {
                Ok(c) => Response::ok(&c),
                Err(e) => Response::db(e),
            }
        }
        ("POST", "/api/relations/create") => {
            let r: RelCreateReq = match req.json() {
                Ok(r) => r,
                Err(e) => return e,
            };
            match store
                .create_manual_edge(r.a, r.b, r.note.as_deref())
                .inspect(|e| state.update_graph(|g| g.upsert_edge(e.clone())))
            {
                Ok(e) => Response::ok(&e),
                Err(e) => Response::db(e),
            }
        }
        ("POST", "/api/relations/delete") => {
            // Manual edges are soft-deleted; automatic edges are hidden from the view.
            let r: EdgeIdReq = match req.json() {
                Ok(r) => r,
                Err(e) => return e,
            };
            let res = store.edge(r.edge_id).and_then(|e| {
                let target = if e.origin == EdgeOrigin::Manual {
                    EdgeState::Deleted
                } else {
                    EdgeState::Hidden
                };
                store.set_edge_state(r.edge_id, target)
            });
            match res.inspect(|e| state.update_graph(|g| g.upsert_edge(e.clone()))) {
                Ok(e) => Response::ok(&e),
                Err(e) => Response::db(e),
            }
        }
        ("POST", "/api/relations/restore") => {
            let r: EdgeIdReq = match req.json() {
                Ok(r) => r,
                Err(e) => return e,
            };
            match store
                .set_edge_state(r.edge_id, EdgeState::Active)
                .inspect(|e| state.update_graph(|g| g.upsert_edge(e.clone())))
            {
                Ok(e) => Response::ok(&e),
                Err(e) => Response::db(e),
            }
        }
        ("POST", "/api/relations/explain") => {
            let r: PairReq = match req.json() {
                Ok(r) => r,
                Err(e) => return e,
            };
            match store.load_edges(None) {
                Ok(all) => {
                    let edges: Vec<_> = all
                        .into_iter()
                        .filter(|e| {
                            (e.src == r.a && e.dst == r.b) || (e.src == r.b && e.dst == r.a)
                        })
                        .collect();
                    let score = combined_score(&edges, state.graph().config());
                    Response::ok(
                        &serde_json::json!({ "a": r.a, "b": r.b, "combined_score": score, "edges": edges }),
                    )
                }
                Err(e) => Response::db(e),
            }
        }
        ("POST", "/api/open-file") => open_file(req, state),
        ("GET", "/api/preview") => preview(req, state),
        ("GET", "/api/recover") => Response::ok(&space_report(&state.graph(), 30)),
        ("GET", "/api/fs/list-dirs") => {
            let p = req
                .query
                .get("path")
                .cloned()
                .or_else(|| std::env::var("HOME").ok())
                .unwrap_or_else(|| "/".into());
            Response::ok(&list_local_directories(Path::new(&p)))
        }
        _ => Response::error(404, "endpoint non trovato"),
    }
}

fn save_positions(req: &Request, state: &AppState) -> Response {
    let r: PosSaveReq = match req.json() {
        Ok(r) => r,
        Err(e) => return e,
    };
    if r.positions.len() > MAX_POSITIONS_PER_SAVE {
        return Response::error(413, "troppe posizioni in una richiesta");
    }
    let g = state.graph();
    let mut updates: Vec<NodePosition> = Vec::new();
    let mut auto_updates: Vec<NodePosition> = Vec::new();
    for p in &r.positions {
        let Some(old) = g.position(p.entity_id) else {
            return Response::error(
                404,
                &format!("posizione sconosciuta per l'entità {}", p.entity_id),
            );
        };
        let mut np = old.clone();
        np.x = p.x;
        np.y = p.y;
        np.source = PositionSource::Manual;
        np.pinned = old.pinned || r.pin;
        let (dx, dy) = (p.x - old.x, p.y - old.y);
        updates.push(np);
        if r.with_descendants && (dx != 0.0 || dy != 0.0) {
            let mut stack: Vec<EntityId> = g.children(p.entity_id).to_vec();
            let mut guard = 0usize;
            while let Some(c) = stack.pop() {
                guard += 1;
                if guard > 200_000 {
                    break;
                }
                if let Some(cp) = g.position(c) {
                    if !cp.is_locked() {
                        let mut t = cp.clone();
                        t.x += dx;
                        t.y += dy;
                        auto_updates.push(t);
                    }
                }
                stack.extend_from_slice(g.children(c));
            }
        }
    }
    let store = state.store();
    let res = store
        .save_manual_positions(state.canvas_id, &updates)
        .and_then(|_| store.save_auto_positions(state.canvas_id, &auto_updates));
    match res {
        Ok(_) => {
            let total = updates.len() + auto_updates.len();
            state.update_graph(|g| {
                g.apply_positions(&updates);
                g.apply_positions(&auto_updates);
            });
            Response::ok(&serde_json::json!({ "saved": total }))
        }
        Err(e) => Response::db(e),
    }
}

const NEVER_OPEN_EXTS: &[&str] = &[
    "desktop",
    "sh",
    "bash",
    "zsh",
    "fish",
    "csh",
    "run",
    "bin",
    "appimage",
    "exe",
    "msi",
    "bat",
    "cmd",
    "com",
    "jar",
    "py",
    "pl",
    "rb",
    "deb",
    "rpm",
    "flatpakref",
    "snap",
    "elf",
    "so",
    "ko",
];

fn open_file(req: &Request, state: &AppState) -> Response {
    let r: OpenReq = match req.json() {
        Ok(r) => r,
        Err(e) => return e,
    };
    let node = match state.graph().node(r.entity_id).cloned() {
        Some(n) => n,
        None => return Response::error(404, "entità non trovata o non presente"),
    };
    let path = raw_bytes_to_path_buf(&node.path_raw);
    // Re-verify on disk: the path must still be the same object that ORBIT observed.
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) => {
            return Response::error(
                409,
                &format!("il file non è più accessibile: {} (riscansiona)", e),
            )
        }
    };
    let kind_ok = match node.kind {
        EntryType::Directory => meta.is_dir(),
        EntryType::File => meta.file_type().is_file(),
        _ => false,
    };
    if !kind_ok || Some(meta.ino()) != node.ino || Some(meta.dev()) != node.dev {
        return Response::error(409, "il percorso ora indica un oggetto diverso da quello osservato: riscansiona prima di aprirlo");
    }
    let (target, mode) = match r.mode.as_str() {
        "reveal" => {
            let dir = if node.kind == EntryType::Directory {
                path.clone()
            } else {
                path.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| path.clone())
            };
            (dir, "reveal")
        }
        "open" => {
            let exec = node.kind == EntryType::File && meta.permissions().mode() & 0o111 != 0;
            let dangerous = NEVER_OPEN_EXTS.contains(&node.extension().as_str());
            if exec || dangerous {
                return Response::ok(&serde_json::json!({
                    "opened": false,
                    "reason": "ORBIT non apre file eseguibili, script o lanciatori: usa «Mostra nella cartella»",
                    "can_reveal": true,
                }));
            }
            (path.clone(), "open")
        }
        _ => return Response::error(400, "mode deve essere 'open' o 'reveal'"),
    };
    match (state.opener)(&target) {
        Ok(()) => Response::ok(
            &serde_json::json!({ "opened": true, "mode": mode, "path": target.to_string_lossy() }),
        ),
        Err(e) => Response::error(500, &format!("impossibile avviare xdg-open: {}", e)),
    }
}

fn preview(req: &Request, state: &AppState) -> Response {
    let Some(id) = req.query_i64("id") else {
        return Response::error(400, "parametro id obbligatorio");
    };
    let Some(node) = state.graph().node(id).cloned() else {
        return Response::error(404, "entità non trovata");
    };
    let kind = preview_kind(&node);
    if kind == PreviewKind::None {
        return Response::error(415, "anteprima non disponibile per questo tipo di file");
    }
    let path = raw_bytes_to_path_buf(&node.path_raw);
    let meta = match fs::symlink_metadata(&path) {
        Ok(m) if m.file_type().is_file() => m,
        _ => return Response::error(409, "file non più disponibile"),
    };
    if Some(meta.ino()) != node.ino {
        return Response::error(409, "il file è cambiato dopo la scansione");
    }
    match kind {
        PreviewKind::Image => {
            if meta.len() > PREVIEW_MAX_IMAGE_BYTES {
                return Response::error(413, "immagine troppo grande per l'anteprima");
            }
            let ct = match node.extension().as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                _ => "image/bmp",
            };
            match fs::read(&path) {
                Ok(b) => Response::raw(200, ct, b),
                Err(e) => Response::error(500, &e.to_string()),
            }
        }
        _ => {
            let mut buf = Vec::with_capacity(PREVIEW_TEXT_BYTES);
            let res = fs::File::open(&path)
                .and_then(|f| f.take(PREVIEW_TEXT_BYTES as u64).read_to_end(&mut buf));
            if let Err(e) = res {
                return Response::error(500, &e.to_string());
            }
            if buf.contains(&0) {
                return Response::error(415, "il file sembra binario");
            }
            Response::ok(&serde_json::json!({
                "kind": "text",
                "text": String::from_utf8_lossy(&buf),
                "truncated": meta.len() as usize > buf.len(),
            }))
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct FsDirListing {
    current_path: String,
    parent_path: Option<String>,
    directories: Vec<FsDirEntry>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct FsDirEntry {
    name: String,
    path: String,
}

fn list_local_directories(path: &Path) -> FsDirListing {
    let target = if path.is_dir() {
        path.to_path_buf()
    } else {
        PathBuf::from("/")
    };
    let canonical = fs::canonicalize(&target).unwrap_or(target);
    let parent_path = canonical.parent().map(|p| p.to_string_lossy().into_owned());
    match fs::read_dir(&canonical) {
        Ok(rd) => {
            let mut dirs: Vec<FsDirEntry> = rd
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| FsDirEntry {
                    name: e.file_name().to_string_lossy().into_owned(),
                    path: e.path().to_string_lossy().into_owned(),
                })
                .collect();
            dirs.sort_by_key(|a| a.name.to_lowercase());
            dirs.truncate(2000);
            FsDirListing {
                current_path: canonical.to_string_lossy().into_owned(),
                parent_path,
                directories: dirs,
                error: None,
            }
        }
        Err(err) => FsDirListing {
            current_path: canonical.to_string_lossy().into_owned(),
            parent_path,
            directories: Vec::new(),
            error: Some(err.to_string()),
        },
    }
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in q.split('&').filter(|p| !p.is_empty()).take(64) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        map.insert(url_decode(k), url_decode(v));
    }
    map
}

fn url_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let h = (bytes[i + 1] as char).to_digit(16);
                let l = (bytes[i + 2] as char).to_digit(16);
                if let (Some(h), Some(l)) = (h, l) {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn write_json<T: Serialize>(
    stream: &mut TcpStream,
    status: u16,
    payload: &T,
) -> std::io::Result<()> {
    write_response(stream, &Response::json(status, payload))
}

fn write_response(stream: &mut TcpStream, resp: &Response) -> std::io::Result<()> {
    let status_text = match resp.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let csp = if resp.content_type.starts_with("text/html") {
        "Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'\r\n"
    } else {
        ""
    };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\nCross-Origin-Resource-Policy: same-origin\r\nX-Orbit-Api: {}\r\n{}Connection: close\r\n\r\n",
        resp.status,
        status_text,
        resp.content_type,
        resp.body.len(),
        API_VERSION,
        csp
    );
    stream.write_all(header.as_bytes())?;
    stream.write_all(&resp.body)?;
    stream.flush()
}

// ================================================================================================
// Desktop launcher
// ================================================================================================

/// Options for [`run`].
pub struct RunOptions {
    pub store: OrbitStore,
    pub port: u16,
    pub initial_scan: Option<PathBuf>,
    pub open_window: bool,
}

/// Starts the server, optionally an initial scan and a desktop window, and blocks.
pub fn run(opts: RunOptions) -> std::io::Result<()> {
    let state = AppState::new(opts.store).map_err(|e| std::io::Error::other(e.to_string()))?;
    if let Some(p) = opts.initial_scan {
        let canonical = fs::canonicalize(&p)?;
        let mut cfg = ScanConfig::new(canonical);
        cfg.progress_interval_ms = 100;
        state
            .start_scan(cfg, IndexOptions::default())
            .map_err(std::io::Error::other)?;
    }
    let server = AppServer::start(opts.port, state)?;
    let url = server.launch_url();
    println!("ORBIT in ascolto su http://{} (solo locale)", server.addr());
    if opts.open_window {
        if let Some(mut child) = launch_desktop_window(&url) {
            let _ = child.wait();
            return Ok(());
        }
    }
    println!(
        "Apri questo indirizzo (valido una sola volta):\n  {}\nCtrl+C per terminare.",
        url
    );
    loop {
        thread::sleep(Duration::from_secs(3600));
    }
}

/// Opens a desktop window on the launch URL: Qt6 WebEngine (`qml6`), then a Chromium-family
/// browser in app mode, then `xdg-open`. Returns the child when the window process stays alive.
fn launch_desktop_window(url: &str) -> Option<std::process::Child> {
    let has_display =
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some();
    if !has_display {
        return None;
    }
    if Path::new("/usr/bin/qml6").exists() {
        // Private file (0600) in a private directory: the launch URL is single-use anyway.
        let dir = std::env::temp_dir().join(format!("orbit-{}", std::process::id()));
        if fs::create_dir_all(&dir).is_ok()
            && fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).is_ok()
        {
            let qml_path = dir.join("window.qml");
            let qml = format!(
                "import QtQuick\nimport QtQuick.Window\nimport QtWebEngine\nWindow {{\n  width: 1400\n  height: 900\n  visible: true\n  title: \"ORBIT\"\n  color: \"#05070d\"\n  WebEngineView {{ anchors.fill: parent; url: {:?} }}\n}}\n",
                url
            );
            let written = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&qml_path)
                .and_then(|mut f| f.write_all(qml.as_bytes()));
            if written.is_ok() {
                if let Ok(mut child) = Command::new("/usr/bin/qml6")
                    .arg(&qml_path)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    thread::sleep(Duration::from_millis(1500));
                    let _ = fs::remove_file(&qml_path);
                    let _ = fs::remove_dir(&dir);
                    if let Ok(None) = child.try_wait() {
                        return Some(child);
                    }
                }
            }
            let _ = fs::remove_dir_all(&dir);
        }
    }
    for bin in [
        "/usr/bin/chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/brave-browser",
        "/usr/bin/brave-browser-stable",
    ] {
        if Path::new(bin).exists() {
            if let Ok(mut child) = Command::new(bin)
                .arg(format!("--app={}", url))
                .arg("--window-size=1400,900")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                thread::sleep(Duration::from_millis(300));
                if let Ok(None) = child.try_wait() {
                    return Some(child);
                }
            }
        }
    }
    let _ = Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    None
}
