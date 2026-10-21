//! Minimal dynamic binding to the system SQLite3 C library (`libsqlite3.so.0`).
//!
//! Inherited from DiskGarden: the library is loaded at runtime with `dlopen`, so ORBIT needs no
//! C toolchain at build time and no bundled SQLite. Only the handful of entry points used by
//! [`crate::store`] are bound. The wrapper is crate-private; the rest of ORBIT talks to
//! [`crate::store::OrbitStore`].

use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::sync::OnceLock;
use thiserror::Error;

/// Errors produced by the SQLite layer and by [`crate::store::OrbitStore`].
#[derive(Debug, Error)]
pub enum DbError {
    #[error("Libreria di sistema SQLite3 (libsqlite3.so.0) non disponibile: {0}")]
    LibraryUnavailable(String),
    #[error("Errore SQLite ({code}): {message}")]
    Sqlite { code: i32, message: String },
    #[error("Errore I/O sul database ORBIT: {0}")]
    Io(#[from] std::io::Error),
    #[error("Versione dello schema del database ({found}) più recente di quella supportata ({supported})")]
    SchemaTooNew { found: i64, supported: i64 },
    #[error("Elemento non trovato: {0}")]
    NotFound(String),
    #[error("Operazione non valida: {0}")]
    Invalid(String),
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_NULL: c_int = 5;
const SQLITE_TRANSIENT: isize = -1;

type Sqlite3Db = c_void;
type Sqlite3Stmt = c_void;

struct SqliteApi {
    open: unsafe extern "C" fn(*const c_char, *mut *mut Sqlite3Db) -> c_int,
    close: unsafe extern "C" fn(*mut Sqlite3Db) -> c_int,
    errmsg: unsafe extern "C" fn(*mut Sqlite3Db) -> *const c_char,
    busy_timeout: unsafe extern "C" fn(*mut Sqlite3Db, c_int) -> c_int,
    exec: unsafe extern "C" fn(
        *mut Sqlite3Db,
        *const c_char,
        Option<
            unsafe extern "C" fn(*mut c_void, c_int, *mut *mut c_char, *mut *mut c_char) -> c_int,
        >,
        *mut c_void,
        *mut *mut c_char,
    ) -> c_int,
    prepare_v2: unsafe extern "C" fn(
        *mut Sqlite3Db,
        *const c_char,
        c_int,
        *mut *mut Sqlite3Stmt,
        *mut *const c_char,
    ) -> c_int,
    step: unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int,
    reset: unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int,
    clear_bindings: unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int,
    finalize: unsafe extern "C" fn(*mut Sqlite3Stmt) -> c_int,
    last_insert_rowid: unsafe extern "C" fn(*mut Sqlite3Db) -> i64,
    changes: unsafe extern "C" fn(*mut Sqlite3Db) -> c_int,
    bind_int64: unsafe extern "C" fn(*mut Sqlite3Stmt, c_int, i64) -> c_int,
    bind_double: unsafe extern "C" fn(*mut Sqlite3Stmt, c_int, f64) -> c_int,
    bind_null: unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> c_int,
    bind_text: unsafe extern "C" fn(*mut Sqlite3Stmt, c_int, *const c_char, c_int, isize) -> c_int,
    bind_blob: unsafe extern "C" fn(*mut Sqlite3Stmt, c_int, *const c_void, c_int, isize) -> c_int,
    column_int64: unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> i64,
    column_double: unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> f64,
    column_text: unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> *const u8,
    column_blob: unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> *const c_void,
    column_bytes: unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> c_int,
    column_type: unsafe extern "C" fn(*mut Sqlite3Stmt, c_int) -> c_int,
}

unsafe impl Send for SqliteApi {}
unsafe impl Sync for SqliteApi {}

static SQLITE_API: OnceLock<Result<SqliteApi, String>> = OnceLock::new();

#[allow(clippy::missing_transmute_annotations)]
fn get_sqlite_api() -> Result<&'static SqliteApi, DbError> {
    let res = SQLITE_API.get_or_init(|| unsafe {
        let candidates = [
            "libsqlite3.so.0\0",
            "libsqlite3.so\0",
            "/usr/lib/x86_64-linux-gnu/libsqlite3.so.0\0",
            "/usr/lib/aarch64-linux-gnu/libsqlite3.so.0\0",
            "/usr/lib64/libsqlite3.so.0\0",
            "/usr/lib/libsqlite3.so.0\0",
        ];
        let mut handle: *mut c_void = ptr::null_mut();
        for cand in candidates {
            handle = libc::dlopen(
                cand.as_ptr() as *const c_char,
                libc::RTLD_NOW | libc::RTLD_LOCAL,
            );
            if !handle.is_null() {
                break;
            }
        }
        if handle.is_null() {
            return Err("Impossibile caricare libsqlite3.so.0 tramite dlopen".to_string());
        }

        macro_rules! load_sym {
            ($name:literal) => {{
                let sym = libc::dlsym(handle, concat!($name, "\0").as_ptr() as *const c_char);
                if sym.is_null() {
                    return Err(format!("Simbolo SQLite mancante: {}", $name));
                }
                std::mem::transmute(sym)
            }};
        }

        Ok(SqliteApi {
            open: load_sym!("sqlite3_open"),
            close: load_sym!("sqlite3_close"),
            errmsg: load_sym!("sqlite3_errmsg"),
            busy_timeout: load_sym!("sqlite3_busy_timeout"),
            exec: load_sym!("sqlite3_exec"),
            prepare_v2: load_sym!("sqlite3_prepare_v2"),
            step: load_sym!("sqlite3_step"),
            reset: load_sym!("sqlite3_reset"),
            clear_bindings: load_sym!("sqlite3_clear_bindings"),
            finalize: load_sym!("sqlite3_finalize"),
            last_insert_rowid: load_sym!("sqlite3_last_insert_rowid"),
            changes: load_sym!("sqlite3_changes"),
            bind_int64: load_sym!("sqlite3_bind_int64"),
            bind_double: load_sym!("sqlite3_bind_double"),
            bind_null: load_sym!("sqlite3_bind_null"),
            bind_text: load_sym!("sqlite3_bind_text"),
            bind_blob: load_sym!("sqlite3_bind_blob"),
            column_int64: load_sym!("sqlite3_column_int64"),
            column_double: load_sym!("sqlite3_column_double"),
            column_text: load_sym!("sqlite3_column_text"),
            column_blob: load_sym!("sqlite3_column_blob"),
            column_bytes: load_sym!("sqlite3_column_bytes"),
            column_type: load_sym!("sqlite3_column_type"),
        })
    });

    match res {
        Ok(api) => Ok(api),
        Err(msg) => Err(DbError::LibraryUnavailable(msg.clone())),
    }
}

/// A single SQLite connection. Not `Send`: open one per thread/operation.
pub(crate) struct Conn {
    db: *mut Sqlite3Db,
    api: &'static SqliteApi,
}

impl Conn {
    pub(crate) fn open(path: &Path) -> Result<Self, DbError> {
        let api = get_sqlite_api()?;
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let c_path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            DbError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "Il percorso del database contiene byte NUL",
            ))
        })?;
        let mut db: *mut Sqlite3Db = ptr::null_mut();
        let rc = unsafe { (api.open)(c_path.as_ptr(), &mut db) };
        if rc != SQLITE_OK || db.is_null() {
            let msg = if !db.is_null() {
                let s = unsafe {
                    CStr::from_ptr((api.errmsg)(db))
                        .to_string_lossy()
                        .into_owned()
                };
                unsafe { (api.close)(db) };
                s
            } else {
                "Impossibile allocare connessione SQLite".to_string()
            };
            return Err(DbError::Sqlite {
                code: rc,
                message: msg,
            });
        }
        unsafe { (api.busy_timeout)(db, 5_000) };
        Ok(Self { db, api })
    }

    fn last_error(&self, code: c_int) -> DbError {
        let msg = unsafe {
            let ptr = (self.api.errmsg)(self.db);
            if ptr.is_null() {
                "Errore SQLite sconosciuto".to_string()
            } else {
                CStr::from_ptr(ptr).to_string_lossy().into_owned()
            }
        };
        DbError::Sqlite { code, message: msg }
    }

    pub(crate) fn exec_batch(&self, sql: &str) -> Result<(), DbError> {
        let c_sql = CString::new(sql)
            .map_err(|_| DbError::Invalid("SQL contenente byte NUL".to_string()))?;
        let rc = unsafe {
            (self.api.exec)(
                self.db,
                c_sql.as_ptr(),
                None,
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK {
            return Err(self.last_error(rc));
        }
        Ok(())
    }

    pub(crate) fn prepare<'a>(&'a self, sql: &str) -> Result<Stmt<'a>, DbError> {
        let c_sql = CString::new(sql)
            .map_err(|_| DbError::Invalid("SQL contenente byte NUL".to_string()))?;
        let mut stmt: *mut Sqlite3Stmt = ptr::null_mut();
        let rc = unsafe {
            (self.api.prepare_v2)(
                self.db,
                c_sql.as_ptr(),
                c_sql.as_bytes().len() as c_int,
                &mut stmt,
                ptr::null_mut(),
            )
        };
        if rc != SQLITE_OK || stmt.is_null() {
            return Err(self.last_error(rc));
        }
        Ok(Stmt { conn: self, stmt })
    }

    pub(crate) fn last_insert_rowid(&self) -> i64 {
        unsafe { (self.api.last_insert_rowid)(self.db) }
    }

    pub(crate) fn changes(&self) -> usize {
        unsafe { (self.api.changes)(self.db).max(0) as usize }
    }

    /// Runs `f` inside a `BEGIN IMMEDIATE` transaction, committing on `Ok` and rolling back on `Err`.
    pub(crate) fn transaction<T>(
        &self,
        f: impl FnOnce(&Conn) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        self.exec_batch("BEGIN IMMEDIATE")?;
        match f(self) {
            Ok(v) => {
                self.exec_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.exec_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Executes a single statement with the given parameters and returns the number of changed rows.
    pub(crate) fn execute(&self, sql: &str, params: &[Value<'_>]) -> Result<usize, DbError> {
        let mut st = self.prepare(sql)?;
        st.bind_all(params)?;
        while st.step()? {}
        Ok(self.changes())
    }

    /// Returns the first column of the first row as `i64`, if any row exists.
    pub(crate) fn query_i64(
        &self,
        sql: &str,
        params: &[Value<'_>],
    ) -> Result<Option<i64>, DbError> {
        let mut st = self.prepare(sql)?;
        st.bind_all(params)?;
        if st.step()? {
            Ok(st.col_opt_i64(0))
        } else {
            Ok(None)
        }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if !self.db.is_null() {
            unsafe {
                (self.api.close)(self.db);
            }
        }
    }
}

/// A bindable SQL parameter value.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Value<'a> {
    Null,
    I64(i64),
    F64(f64),
    Text(&'a str),
    Blob(&'a [u8]),
}

impl From<i64> for Value<'_> {
    fn from(v: i64) -> Self {
        Value::I64(v)
    }
}

impl From<f64> for Value<'_> {
    fn from(v: f64) -> Self {
        Value::F64(v)
    }
}

impl From<bool> for Value<'_> {
    fn from(v: bool) -> Self {
        Value::I64(i64::from(v))
    }
}

impl<'a> From<&'a str> for Value<'a> {
    fn from(v: &'a str) -> Self {
        Value::Text(v)
    }
}

impl<'a> From<&'a [u8]> for Value<'a> {
    fn from(v: &'a [u8]) -> Self {
        Value::Blob(v)
    }
}

impl<'a, T: Into<Value<'a>>> From<Option<T>> for Value<'a> {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(x) => x.into(),
            None => Value::Null,
        }
    }
}

pub(crate) struct Stmt<'a> {
    conn: &'a Conn,
    stmt: *mut Sqlite3Stmt,
}

impl Stmt<'_> {
    pub(crate) fn reset(&mut self) {
        unsafe {
            (self.conn.api.reset)(self.stmt);
            (self.conn.api.clear_bindings)(self.stmt);
        }
    }

    fn check(&self, rc: c_int) -> Result<(), DbError> {
        if rc != SQLITE_OK {
            return Err(self.conn.last_error(rc));
        }
        Ok(())
    }

    /// Binds `params` to positions `1..=params.len()`.
    pub(crate) fn bind_all(&mut self, params: &[Value<'_>]) -> Result<(), DbError> {
        for (i, p) in params.iter().enumerate() {
            self.bind(i as c_int + 1, *p)?;
        }
        Ok(())
    }

    pub(crate) fn bind(&mut self, idx: c_int, val: Value<'_>) -> Result<(), DbError> {
        let rc = unsafe {
            match val {
                Value::Null => (self.conn.api.bind_null)(self.stmt, idx),
                Value::I64(v) => (self.conn.api.bind_int64)(self.stmt, idx, v),
                Value::F64(v) => (self.conn.api.bind_double)(self.stmt, idx, v),
                Value::Text(s) => (self.conn.api.bind_text)(
                    self.stmt,
                    idx,
                    s.as_ptr() as *const c_char,
                    s.len() as c_int,
                    SQLITE_TRANSIENT,
                ),
                Value::Blob(bytes) => {
                    // SQLite treats a NULL pointer with len 0 as SQL NULL: pass a non-null pointer
                    // so that an empty path (the scan root) is stored as X'' and not NULL.
                    let non_null_ptr = if bytes.is_empty() {
                        b"" as *const u8 as *const c_void
                    } else {
                        bytes.as_ptr() as *const c_void
                    };
                    (self.conn.api.bind_blob)(
                        self.stmt,
                        idx,
                        non_null_ptr,
                        bytes.len() as c_int,
                        SQLITE_TRANSIENT,
                    )
                }
            }
        };
        self.check(rc)
    }

    pub(crate) fn step(&mut self) -> Result<bool, DbError> {
        let rc = unsafe { (self.conn.api.step)(self.stmt) };
        if rc == SQLITE_ROW {
            Ok(true)
        } else if rc == SQLITE_DONE {
            Ok(false)
        } else {
            Err(self.conn.last_error(rc))
        }
    }

    /// Resets, binds `params`, and runs the statement to completion (for INSERT/UPDATE/DELETE).
    pub(crate) fn run(&mut self, params: &[Value<'_>]) -> Result<(), DbError> {
        self.reset();
        self.bind_all(params)?;
        while self.step()? {}
        Ok(())
    }

    fn is_null(&self, col: c_int) -> bool {
        unsafe { (self.conn.api.column_type)(self.stmt, col) == SQLITE_NULL }
    }

    pub(crate) fn col_i64(&self, col: c_int) -> i64 {
        unsafe { (self.conn.api.column_int64)(self.stmt, col) }
    }

    pub(crate) fn col_opt_i64(&self, col: c_int) -> Option<i64> {
        if self.is_null(col) {
            None
        } else {
            Some(self.col_i64(col))
        }
    }

    pub(crate) fn col_f64(&self, col: c_int) -> f64 {
        unsafe { (self.conn.api.column_double)(self.stmt, col) }
    }

    pub(crate) fn col_bool(&self, col: c_int) -> bool {
        self.col_i64(col) != 0
    }

    pub(crate) fn col_text(&self, col: c_int) -> String {
        unsafe {
            let ptr = (self.conn.api.column_text)(self.stmt, col);
            let len = (self.conn.api.column_bytes)(self.stmt, col);
            if ptr.is_null() || len <= 0 {
                String::new()
            } else {
                let slice = std::slice::from_raw_parts(ptr, len as usize);
                String::from_utf8_lossy(slice).into_owned()
            }
        }
    }

    pub(crate) fn col_opt_text(&self, col: c_int) -> Option<String> {
        if self.is_null(col) {
            None
        } else {
            Some(self.col_text(col))
        }
    }

    pub(crate) fn col_blob(&self, col: c_int) -> Vec<u8> {
        unsafe {
            let ptr = (self.conn.api.column_blob)(self.stmt, col);
            let len = (self.conn.api.column_bytes)(self.stmt, col);
            if ptr.is_null() || len <= 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(ptr as *const u8, len as usize).to_vec()
            }
        }
    }
}

impl Drop for Stmt<'_> {
    fn drop(&mut self) {
        if !self.stmt.is_null() {
            unsafe {
                (self.conn.api.finalize)(self.stmt);
            }
        }
    }
}
