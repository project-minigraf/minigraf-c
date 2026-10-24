#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_sign_loss,
    )
)]

use minigraf::{MinigrafError, QueryResult, Value};
use serde::{Deserialize, Serialize};
use std::ffi::{CStr, CString};
use std::os::raw::{c_char, c_int};
use std::sync::{Arc, Mutex, Weak};

/// The last error of an object, readable as a C string until the next call.
#[derive(Default)]
struct LastError(Mutex<Option<CString>>);

impl LastError {
    fn set(&self, msg: String) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = Some(c_string(msg));
        }
    }

    fn clear(&self) {
        if let Ok(mut guard) = self.0.lock() {
            *guard = None;
        }
    }

    fn as_ptr(&self) -> *const c_char {
        match self.0.lock() {
            Ok(guard) => guard.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()),
            Err(_) => std::ptr::null(),
        }
    }
}

fn c_string(s: String) -> CString {
    CString::new(s).unwrap_or_else(|_| CString::new("error").unwrap_or_default())
}

/// Read a C string argument as UTF-8, or an API-017 error naming it.
fn arg_str<'a>(p: *const c_char, what: &str) -> Result<&'a str, MinigrafError> {
    if p.is_null() {
        return Err(MinigrafError::invalid_argument(format!("{what} is NULL")));
    }
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| MinigrafError::invalid_argument(format!("{what} is not UTF-8")))
}

/// Store `msg` in `*error_out` (freed with `minigraf_string_free`) if it is set.
fn report(error_out: *mut *mut c_char, msg: String) {
    if !error_out.is_null() {
        unsafe { *error_out = c_string(msg).into_raw() };
    }
}

fn json_arg<T: serde::de::DeserializeOwned + Default>(
    p: *const c_char,
    what: &str,
) -> Result<T, MinigrafError> {
    if p.is_null() {
        return Ok(T::default());
    }
    serde_json::from_str(arg_str(p, what)?)
        .map_err(|e| MinigrafError::invalid_argument(format!("{what}: {e}")))
}

fn to_c_json<T: Serialize>(value: &T) -> *mut c_char {
    match serde_json::to_string(value) {
        Ok(s) => c_string(s).into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

// ─── Handle ──────────────────────────────────────────────────────────────────

pub struct MiniGrafDb {
    db: Mutex<minigraf::Minigraf>,
    last_error: Mutex<Option<CString>>,
    /// Every prepared query made from this handle, emptied when it is closed
    /// so that none keeps the file open.
    prepared: Mutex<Vec<Weak<PreparedSlot>>>,
}

impl Drop for MiniGrafDb {
    fn drop(&mut self) {
        // A prepared query holds the file open too (core's PreparedQuery keeps
        // the storage), so release every one before the database itself.
        if let Ok(prepared) = self.prepared.get_mut() {
            for slot in prepared.drain(..).filter_map(|w| w.upgrade()) {
                if let Ok(mut q) = slot.lock() {
                    *q = None;
                }
            }
        }
    }
}

impl MiniGrafDb {
    fn set_error(&self, msg: String) {
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = Some(
                CString::new(msg).unwrap_or_else(|_| CString::new("error").unwrap_or_default()),
            );
        }
    }

    fn clear_error(&self) {
        if let Ok(mut guard) = self.last_error.lock() {
            *guard = None;
        }
    }
}

// ─── Lifecycle ────────────────────────────────────────────────────────────────

/// Open a file-backed Minigraf database. Returns NULL on error.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_open(path: *const c_char) -> *mut MiniGrafDb {
    if path.is_null() {
        return std::ptr::null_mut();
    }
    let path = match unsafe { CStr::from_ptr(path) }.to_str() {
        Ok(s) => s,
        Err(_) => return std::ptr::null_mut(),
    };
    match minigraf::Minigraf::open(path) {
        Ok(db) => Box::into_raw(Box::new(MiniGrafDb {
            db: Mutex::new(db),
            last_error: Mutex::new(None),
            prepared: Mutex::default(),
        })),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Open an in-memory Minigraf database. Returns NULL on error.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_open_in_memory() -> *mut MiniGrafDb {
    match minigraf::Minigraf::in_memory() {
        Ok(db) => Box::into_raw(Box::new(MiniGrafDb {
            db: Mutex::new(db),
            last_error: Mutex::new(None),
            prepared: Mutex::default(),
        })),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Close a database and free all associated memory.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_close(handle: *mut MiniGrafDb) {
    if !handle.is_null() {
        unsafe { drop(Box::from_raw(handle)) };
    }
}

// ─── Execute ─────────────────────────────────────────────────────────────────

/// Execute a Datalog string. Returns a JSON string on success (caller must free
/// with `minigraf_string_free`), or NULL on error (call `minigraf_last_error`).
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_execute(handle: *mut MiniGrafDb, datalog: *const c_char) -> *mut c_char {
    if handle.is_null() || datalog.is_null() {
        return std::ptr::null_mut();
    }
    let handle = unsafe { &*handle };
    let datalog = match unsafe { CStr::from_ptr(datalog) }.to_str() {
        Ok(s) => s,
        Err(_) => {
            handle.set_error("invalid UTF-8 in datalog string".into());
            return std::ptr::null_mut();
        }
    };

    let db_guard = match handle.db.lock() {
        Ok(g) => g,
        Err(_) => {
            handle.set_error("database lock poisoned".into());
            return std::ptr::null_mut();
        }
    };
    let result = db_guard.execute(datalog);
    drop(db_guard);
    match result {
        Ok(qr) => {
            handle.clear_error();
            let json = query_result_to_json(qr);
            match CString::new(json) {
                Ok(s) => s.into_raw(),
                Err(_) => {
                    handle.set_error("result JSON contained a null byte".into());
                    std::ptr::null_mut()
                }
            }
        }
        Err(e) => {
            handle.set_error(format!("{e:#}"));
            std::ptr::null_mut()
        }
    }
}

/// Free a string returned by `minigraf_execute`.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_string_free(s: *mut c_char) {
    if !s.is_null() {
        unsafe { drop(CString::from_raw(s)) };
    }
}

// ─── Checkpoint ───────────────────────────────────────────────────────────────

/// Flush the WAL to the database file. Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_checkpoint(handle: *mut MiniGrafDb) -> c_int {
    if handle.is_null() {
        return -1;
    }
    let handle = unsafe { &*handle };
    let db_guard = match handle.db.lock() {
        Ok(g) => g,
        Err(_) => {
            handle.set_error("database lock poisoned".into());
            return -1;
        }
    };
    match db_guard.checkpoint() {
        Ok(_) => {
            handle.clear_error();
            0
        }
        Err(e) => {
            handle.set_error(format!("{e:#}"));
            -1
        }
    }
}

// ─── Open options ─────────────────────────────────────────────────────────────

/// `options_json`: every key optional, an absent key keeps the Rust default.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenOptionsJson {
    read_only: Option<bool>,
    page_cache_size: Option<usize>,
    allow_unlocked: Option<bool>,
    wal_checkpoint_threshold: Option<usize>,
    max_derived_facts: Option<usize>,
    max_results: Option<usize>,
    synchronous: Option<SyncModeJson>,
    query_memory_limit: Option<usize>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum SyncModeJson {
    Full,
    Normal,
}

impl OpenOptionsJson {
    fn to_core(&self) -> minigraf::OpenOptions {
        let mut o = minigraf::OpenOptions::new();
        if let Some(v) = self.read_only {
            o = o.read_only(v);
        }
        if let Some(v) = self.page_cache_size {
            o = o.page_cache_size(v);
        }
        if let Some(v) = self.allow_unlocked {
            o = o.allow_unlocked(v);
        }
        if let Some(v) = self.wal_checkpoint_threshold {
            o = o.wal_checkpoint_threshold(v);
        }
        if let Some(v) = self.max_derived_facts {
            o = o.max_derived_facts(v);
        }
        if let Some(v) = self.max_results {
            #[allow(deprecated)]
            {
                o = o.max_results(v);
            }
        }
        if let Some(v) = self.query_memory_limit {
            o = o.query_memory_limit(v);
        }
        if let Some(v) = self.synchronous {
            o = o.synchronous(match v {
                SyncModeJson::Full => minigraf::SyncMode::Full,
                SyncModeJson::Normal => minigraf::SyncMode::Normal,
            });
        }
        o
    }
}

fn open_options(options_json: *const c_char) -> Result<minigraf::OpenOptions, MinigrafError> {
    Ok(json_arg::<OpenOptionsJson>(options_json, "options_json")?.to_core())
}

/// Open a file-backed database with `options_json`, a JSON object with any of
/// `read_only`, `page_cache_size`, `allow_unlocked`, `wal_checkpoint_threshold`,
/// `max_derived_facts`, `max_results` (a deprecated alias of
/// `max_derived_facts`), `synchronous` (`"full"` or `"normal"`) and
/// `query_memory_limit` (the most bytes one query may hold, else `QRY-012`;
/// default no limit); NULL means all defaults. Returns NULL on error and, if
/// `error_out` is not NULL, stores the message there (free it with
/// `minigraf_string_free`).
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_open_with_options(
    path: *const c_char,
    options_json: *const c_char,
    error_out: *mut *mut c_char,
) -> *mut MiniGrafDb {
    let opened = arg_str(path, "path")
        .and_then(|path| minigraf::Minigraf::open_with_options(path, open_options(options_json)?));
    match opened {
        Ok(db) => Box::into_raw(Box::new(MiniGrafDb {
            db: Mutex::new(db),
            last_error: Mutex::new(None),
            prepared: Mutex::default(),
        })),
        Err(e) => {
            report(error_out, e.to_string());
            std::ptr::null_mut()
        }
    }
}

/// The transaction counter that `:as-of N` compares against, in `*out`.
/// Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_current_tx_count(handle: *mut MiniGrafDb, out: *mut u64) -> c_int {
    if handle.is_null() || out.is_null() {
        return -1;
    }
    let handle = unsafe { &*handle };
    match handle.db.lock() {
        Ok(db) => {
            handle.clear_error();
            unsafe { *out = db.current_tx_count() };
            0
        }
        Err(_) => {
            handle.set_error("database lock poisoned".into());
            -1
        }
    }
}

// ─── Cursor ───────────────────────────────────────────────────────────────────

pub struct MiniGrafCursor {
    vars: Vec<String>,
    /// `None` after the end.
    cursor: Mutex<Option<minigraf::Cursor>>,
    last_error: LastError,
}

/// Open a cursor over a `(query ...)`; its answer is fixed when it opens.
/// Returns NULL on error (call `minigraf_last_error` on `handle`). Free the
/// cursor with `minigraf_cursor_free`.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_query(
    handle: *mut MiniGrafDb,
    datalog: *const c_char,
) -> *mut MiniGrafCursor {
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    let handle = unsafe { &*handle };
    let result = arg_str(datalog, "datalog")
        .map_err(|e| e.to_string())
        .and_then(|q| match handle.db.lock() {
            Ok(db) => db.query(q).map_err(|e| e.to_string()),
            Err(_) => Err("database lock poisoned".to_string()),
        });
    match result {
        Ok(cursor) => {
            handle.clear_error();
            Box::into_raw(Box::new(MiniGrafCursor {
                vars: cursor.vars().to_vec(),
                cursor: Mutex::new(Some(cursor)),
                last_error: LastError::default(),
            }))
        }
        Err(e) => {
            handle.set_error(e);
            std::ptr::null_mut()
        }
    }
}

/// The `:find` variables as a JSON array of strings. Free with
/// `minigraf_string_free`.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_cursor_vars(cursor: *mut MiniGrafCursor) -> *mut c_char {
    if cursor.is_null() {
        return std::ptr::null_mut();
    }
    to_c_json(&unsafe { &*cursor }.vars)
}

/// The next batch of at most `max_rows` rows (0 counts as 1) as a JSON array of
/// rows, encoded like `minigraf_execute`'s `results`; free it with
/// `minigraf_string_free`. Returns NULL at the end, or on error, when
/// `minigraf_cursor_last_error` is not NULL.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_cursor_next_batch(
    cursor: *mut MiniGrafCursor,
    max_rows: usize,
) -> *mut c_char {
    if cursor.is_null() {
        return std::ptr::null_mut();
    }
    let cursor = unsafe { &*cursor };
    cursor.last_error.clear();
    let mut guard = match cursor.cursor.lock() {
        Ok(g) => g,
        Err(_) => {
            cursor.last_error.set("cursor lock poisoned".into());
            return std::ptr::null_mut();
        }
    };
    let Some(c) = guard.as_mut() else {
        return std::ptr::null_mut();
    };
    match c.next_batch(max_rows) {
        Ok(Some(batch)) => c_string(rows_to_json(batch.rows())).into_raw(),
        Ok(None) => {
            *guard = None;
            std::ptr::null_mut()
        }
        Err(e) => {
            cursor.last_error.set(e.to_string());
            std::ptr::null_mut()
        }
    }
}

/// The error of the last `minigraf_cursor_next_batch`, or NULL if it had none.
/// Valid until the next call on the cursor.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_cursor_last_error(cursor: *mut MiniGrafCursor) -> *const c_char {
    if cursor.is_null() {
        return std::ptr::null();
    }
    unsafe { &*cursor }.last_error.as_ptr()
}

/// Close a cursor and free it.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_cursor_free(cursor: *mut MiniGrafCursor) {
    if !cursor.is_null() {
        unsafe { drop(Box::from_raw(cursor)) };
    }
}

// ─── Prepared queries ─────────────────────────────────────────────────────────

/// A prepared query, `None` once its database is closed.
/// cbindgen:ignore
type PreparedSlot = Mutex<Option<minigraf::PreparedQuery>>;

pub struct MiniGrafPrepared {
    slot: Arc<PreparedSlot>,
    last_error: LastError,
}

/// Parse and validate a `(query ...)` once, for running many times with
/// different bind values in its `$name` slots. Returns NULL on error (call
/// `minigraf_last_error` on `handle`): API-005/006/007 for a transact, retract
/// or rule, API-023 for a slot in an attribute position. Free it with
/// `minigraf_prepared_free`, also after `minigraf_close`.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_prepare(
    handle: *mut MiniGrafDb,
    datalog: *const c_char,
) -> *mut MiniGrafPrepared {
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    let handle = unsafe { &*handle };
    let result = arg_str(datalog, "datalog")
        .map_err(|e| e.to_string())
        .and_then(|q| match handle.db.lock() {
            Ok(db) => db.prepare(q).map_err(|e| e.to_string()),
            Err(_) => Err("database lock poisoned".to_string()),
        })
        .and_then(|q| {
            let slot = Arc::new(Mutex::new(Some(q)));
            let mut prepared = handle
                .prepared
                .lock()
                .map_err(|_| "database lock poisoned".to_string())?;
            prepared.retain(|w| w.strong_count() > 0);
            prepared.push(Arc::downgrade(&slot));
            Ok(slot)
        });
    match result {
        Ok(slot) => {
            handle.clear_error();
            Box::into_raw(Box::new(MiniGrafPrepared {
                slot,
                last_error: LastError::default(),
            }))
        }
        Err(e) => {
            handle.set_error(e);
            std::ptr::null_mut()
        }
    }
}

/// Run a prepared query with `bindings_json`, a JSON object of slot name
/// (without the `$`) to bind value; NULL means `{}`. A bind value is `true`/
/// `false`, a number (an integer when it has no fraction or exponent, else a
/// float), a string, `null`, or a typed object: `{"type": "integer" | "float" |
/// "string" | "boolean" | "keyword" | "ref" | "null", "value": ...}`,
/// `{"type": "entity", "value": uuid}`, `{"type": "txCount" | "timestamp",
/// "value": n}` or `{"type": "anyValidTime"}`. Returns the result as JSON,
/// encoded like `minigraf_execute`'s (free it with `minigraf_string_free`), or
/// NULL on error (call `minigraf_prepared_last_error`): API-017 for bindings
/// that are not bind values, API-022 for a missing slot, API-024 for a bind
/// value of the wrong kind, API-018 after `minigraf_close` of its database.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_prepared_execute(
    prepared: *mut MiniGrafPrepared,
    bindings_json: *const c_char,
) -> *mut c_char {
    if prepared.is_null() {
        return std::ptr::null_mut();
    }
    let prepared = unsafe { &*prepared };
    match prepared.run(bindings_json, |q, b| q.execute(b)) {
        Ok(result) => c_string(query_result_to_json(result)).into_raw(),
        Err(()) => std::ptr::null_mut(),
    }
}

/// Run a prepared query with `bindings_json` (as `minigraf_prepared_execute`)
/// and return a cursor over its rows; its answer is fixed when it opens.
/// Returns NULL on error (call `minigraf_prepared_last_error`). Free the cursor
/// with `minigraf_cursor_free`.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_prepared_query(
    prepared: *mut MiniGrafPrepared,
    bindings_json: *const c_char,
) -> *mut MiniGrafCursor {
    if prepared.is_null() {
        return std::ptr::null_mut();
    }
    let prepared = unsafe { &*prepared };
    match prepared.run(bindings_json, |q, b| q.query(b)) {
        Ok(cursor) => Box::into_raw(Box::new(MiniGrafCursor {
            vars: cursor.vars().to_vec(),
            cursor: Mutex::new(Some(cursor)),
            last_error: LastError::default(),
        })),
        Err(()) => std::ptr::null_mut(),
    }
}

/// The error of the last call on `prepared`, or NULL if it had none. Valid
/// until the next call on it.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_prepared_last_error(prepared: *mut MiniGrafPrepared) -> *const c_char {
    if prepared.is_null() {
        return std::ptr::null();
    }
    unsafe { &*prepared }.last_error.as_ptr()
}

/// Free a prepared query. Call it also after `minigraf_close` of its database.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_prepared_free(prepared: *mut MiniGrafPrepared) {
    if !prepared.is_null() {
        unsafe { drop(Box::from_raw(prepared)) };
    }
}

impl MiniGrafPrepared {
    /// Read the bindings and run `f`; on an error, set `last_error`.
    fn run<T>(
        &self,
        bindings_json: *const c_char,
        f: impl FnOnce(
            &minigraf::PreparedQuery,
            &[(&str, minigraf::BindValue)],
        ) -> Result<T, MinigrafError>,
    ) -> Result<T, ()> {
        self.last_error.clear();
        let result = read_bindings(bindings_json).and_then(|binds| {
            let slice: Vec<(&str, minigraf::BindValue)> =
                binds.iter().map(|(n, v)| (n.as_str(), v.clone())).collect();
            let guard = self
                .slot
                .lock()
                .map_err(|_| MinigrafError::invalid_argument("prepared query lock poisoned"))?;
            let q = guard
                .as_ref()
                .ok_or_else(|| MinigrafError::closed("database"))?;
            f(q, &slice)
        });
        result.map_err(|e| self.last_error.set(e.to_string()))
    }
}

/// `bindings_json` as `(slot, bind value)` pairs; NULL means none.
fn read_bindings(
    bindings_json: *const c_char,
) -> Result<Vec<(String, minigraf::BindValue)>, MinigrafError> {
    if bindings_json.is_null() {
        return Ok(Vec::new());
    }
    let text = arg_str(bindings_json, "bindings_json")?;
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| MinigrafError::invalid_argument(format!("bindings_json: {e}")))?;
    let serde_json::Value::Object(map) = value else {
        return Err(MinigrafError::invalid_argument(
            "bindings_json must be a JSON object of slot name to bind value",
        ));
    };
    map.into_iter()
        .map(|(name, v)| {
            let bind = bind_value(&name, &v)?;
            Ok((name, bind))
        })
        .collect()
}

fn bind_value(name: &str, v: &serde_json::Value) -> Result<minigraf::BindValue, MinigrafError> {
    use minigraf::BindValue as B;
    use serde_json::Value as J;
    let what = format!("slot ${name}");
    Ok(match v {
        J::Bool(b) => B::Val(Value::Boolean(*b)),
        J::Null => B::Val(Value::Null),
        J::String(s) => B::Val(Value::String(s.clone())),
        J::Number(n) => B::Val(number(&what, n)?),
        J::Object(o) => match o.get("type").and_then(J::as_str) {
            Some(kind) => typed_value(&format!("{what} of type {kind:?}"), kind, o.get("value"))?,
            None => return Err(not_a_bind_value(&what)),
        },
        J::Array(_) => return Err(not_a_bind_value(&what)),
    })
}

fn not_a_bind_value(what: &str) -> MinigrafError {
    MinigrafError::invalid_argument(format!(
        "{what}: a bind value is a boolean, number, string, null or {{\"type\", \"value\"}} object"
    ))
}

/// A JSON number: an integer when it has no fraction or exponent, else a float.
fn number(what: &str, n: &serde_json::Number) -> Result<Value, MinigrafError> {
    if let Some(i) = n.as_i64() {
        Ok(Value::Integer(i))
    } else if n.is_u64() {
        Err(MinigrafError::invalid_argument(format!(
            "{what}: {n} does not fit in a signed 64-bit integer"
        )))
    } else {
        Ok(Value::Float(n.as_f64().unwrap_or(f64::NAN)))
    }
}

fn typed_value(
    what: &str,
    kind: &str,
    value: Option<&serde_json::Value>,
) -> Result<minigraf::BindValue, MinigrafError> {
    use minigraf::BindValue as B;
    use serde_json::Value as J;
    let mismatch =
        || MinigrafError::invalid_argument(format!("{what}: the value does not match the type"));
    let string = || match value {
        Some(J::String(s)) => Ok(s.clone()),
        _ => Err(mismatch()),
    };
    let integer = || match value {
        Some(J::Number(n)) => n.as_i64().ok_or_else(mismatch),
        _ => Err(mismatch()),
    };
    let uuid = |s: String| {
        minigraf::EntityId::parse_str(&s)
            .map_err(|_| MinigrafError::invalid_argument(format!("{what}: {s:?} is not a UUID")))
    };
    Ok(match kind {
        "string" => B::Val(Value::String(string()?)),
        "keyword" => B::Val(Value::Keyword(string()?)),
        "boolean" => match value {
            Some(J::Bool(b)) => B::Val(Value::Boolean(*b)),
            _ => return Err(mismatch()),
        },
        "integer" => B::Val(Value::Integer(integer()?)),
        "float" => match value {
            Some(J::Number(n)) => B::Val(Value::Float(n.as_f64().ok_or_else(mismatch)?)),
            _ => return Err(mismatch()),
        },
        "ref" => B::Val(Value::Ref(uuid(string()?)?)),
        "entity" => B::Entity(uuid(string()?)?),
        "txCount" => {
            let n = integer()?;
            B::TxCount(
                u64::try_from(n).map_err(|_| {
                    MinigrafError::invalid_argument(format!("{what}: {n} is negative"))
                })?,
            )
        }
        "timestamp" => B::Timestamp(integer()?),
        // These two take no value.
        "null" if value.is_none() => B::Val(Value::Null),
        "anyValidTime" if value.is_none() => B::AnyValidTime,
        "null" | "anyValidTime" => return Err(mismatch()),
        _ => {
            return Err(MinigrafError::invalid_argument(format!(
                "{what}: unknown bind value type"
            )));
        }
    })
}

// ─── Fact records ─────────────────────────────────────────────────────────────

/// A value with its type kept: `{"type": "string", "value": "x"}`, with types
/// `string`, `integer`, `float`, `boolean`, `ref` (a UUID string), `keyword`
/// and `null` (no `value`).
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
enum ValueJson {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Ref(String),
    Keyword(String),
    Null,
}

impl From<Value> for ValueJson {
    fn from(v: Value) -> Self {
        match v {
            Value::String(s) => ValueJson::String(s),
            Value::Integer(i) => ValueJson::Integer(i),
            Value::Float(f) => ValueJson::Float(f),
            Value::Boolean(b) => ValueJson::Boolean(b),
            Value::Ref(id) => ValueJson::Ref(id.to_string()),
            Value::Keyword(k) => ValueJson::Keyword(k),
            Value::Null => ValueJson::Null,
        }
    }
}

fn parse_uuid(s: &str, what: &str) -> Result<minigraf::EntityId, MinigrafError> {
    minigraf::EntityId::parse_str(s)
        .map_err(|_| MinigrafError::invalid_argument(format!("{what} {s:?} is not a UUID")))
}

impl ValueJson {
    fn to_core(&self) -> Result<Value, MinigrafError> {
        Ok(match self {
            ValueJson::String(s) => Value::String(s.clone()),
            ValueJson::Integer(i) => Value::Integer(*i),
            ValueJson::Float(f) => Value::Float(*f),
            ValueJson::Boolean(b) => Value::Boolean(*b),
            ValueJson::Ref(s) => Value::Ref(parse_uuid(s, "ref value")?),
            ValueJson::Keyword(k) => Value::Keyword(k.clone()),
            ValueJson::Null => Value::Null,
        })
    }
}

/// One fact-log record. `valid_to` = 9223372036854775807 is forever.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FactRecordJson {
    entity: String,
    attribute: String,
    value: ValueJson,
    tx_count: u64,
    tx_id: u64,
    valid_from: i64,
    valid_to: i64,
    asserted: bool,
}

impl From<minigraf::FactRecord> for FactRecordJson {
    fn from(r: minigraf::FactRecord) -> Self {
        FactRecordJson {
            entity: r.entity.to_string(),
            attribute: r.attribute,
            value: r.value.into(),
            tx_count: r.tx_count,
            tx_id: r.tx_id,
            valid_from: r.valid_from,
            valid_to: r.valid_to,
            asserted: r.asserted,
        }
    }
}

impl FactRecordJson {
    fn to_core(&self) -> Result<minigraf::FactRecord, MinigrafError> {
        Ok(minigraf::FactRecord {
            entity: parse_uuid(&self.entity, "entity")?,
            attribute: self.attribute.clone(),
            value: self.value.to_core()?,
            tx_count: self.tx_count,
            tx_id: self.tx_id,
            valid_from: self.valid_from,
            valid_to: self.valid_to,
            asserted: self.asserted,
        })
    }
}

/// `filter_json`: every key optional, an absent key does not filter.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct FactFilterJson {
    attributes: Option<Vec<String>>,
    attribute_prefixes: Option<Vec<String>>,
    entities: Option<Vec<String>>,
    tx_from: Option<u64>,
    tx_to: Option<u64>,
    order: Option<FactOrderJson>,
    window: Option<usize>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum FactOrderJson {
    Tx,
    Storage,
}

impl FactFilterJson {
    fn to_core(&self) -> Result<minigraf::FactFilter, MinigrafError> {
        let mut f = minigraf::FactFilter::new();
        if let Some(attrs) = &self.attributes {
            f = f.attributes(attrs.iter().cloned());
        }
        for prefix in self.attribute_prefixes.iter().flatten() {
            f = f.attribute_prefix(prefix);
        }
        if let Some(entities) = &self.entities {
            let ids = entities
                .iter()
                .map(|e| parse_uuid(e, "entity"))
                .collect::<Result<Vec<_>, _>>()?;
            f = f.entities(ids);
        }
        if self.tx_from.is_some() || self.tx_to.is_some() {
            f = f.tx_range(self.tx_from.unwrap_or(0)..=self.tx_to.unwrap_or(u64::MAX));
        }
        if let Some(order) = self.order {
            f = f.order(match order {
                FactOrderJson::Tx => minigraf::FactOrder::Tx,
                FactOrderJson::Storage => minigraf::FactOrder::Storage,
            });
        }
        if let Some(w) = self.window {
            f = f.window(w);
        }
        Ok(f)
    }
}

// ─── Fact log ─────────────────────────────────────────────────────────────────

pub struct MiniGrafFactLog {
    /// `None` after the end, which releases the database.
    log: Mutex<Option<minigraf::FactLog>>,
    last_error: LastError,
}

/// Stream every fact record that `filter_json` keeps: a JSON object with any of
/// `attributes`, `attribute_prefixes`, `entities` (UUID strings), `tx_from` and
/// `tx_to` (inclusive), `order` (`"tx"` or `"storage"`) and `window`; NULL keeps
/// every record. Checkpoints wait until the log ends or is freed. Returns NULL
/// on error (call `minigraf_last_error` on `handle`).
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_fact_log(
    handle: *mut MiniGrafDb,
    filter_json: *const c_char,
) -> *mut MiniGrafFactLog {
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    let handle = unsafe { &*handle };
    let result = json_arg::<FactFilterJson>(filter_json, "filter_json")
        .and_then(|f| f.to_core())
        .map_err(|e| e.to_string())
        .and_then(|filter| match handle.db.lock() {
            Ok(db) => db.fact_log(&filter).map_err(|e| e.to_string()),
            Err(_) => Err("database lock poisoned".to_string()),
        });
    match result {
        Ok(log) => {
            handle.clear_error();
            Box::into_raw(Box::new(MiniGrafFactLog {
                log: Mutex::new(Some(log)),
                last_error: LastError::default(),
            }))
        }
        Err(e) => {
            handle.set_error(e);
            std::ptr::null_mut()
        }
    }
}

/// The next batch of at most `max_records` records (0 counts as 1) as a JSON
/// array of records: `{"entity", "attribute", "value": {"type", "value"},
/// "tx_count", "tx_id", "valid_from", "valid_to", "asserted"}`. Free it with
/// `minigraf_string_free`. Returns NULL at the end, or on error, when
/// `minigraf_fact_log_last_error` is not NULL.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_fact_log_next_batch(
    log: *mut MiniGrafFactLog,
    max_records: usize,
) -> *mut c_char {
    if log.is_null() {
        return std::ptr::null_mut();
    }
    let log = unsafe { &*log };
    log.last_error.clear();
    let mut guard = match log.log.lock() {
        Ok(g) => g,
        Err(_) => {
            log.last_error.set("fact log lock poisoned".into());
            return std::ptr::null_mut();
        }
    };
    let Some(l) = guard.as_mut() else {
        return std::ptr::null_mut();
    };
    match l.next_batch(max_records) {
        Ok(Some(batch)) => {
            let records: Vec<FactRecordJson> = batch.into_iter().map(Into::into).collect();
            to_c_json(&records)
        }
        Ok(None) => {
            *guard = None;
            std::ptr::null_mut()
        }
        Err(e) => {
            *guard = None;
            log.last_error.set(e.to_string());
            std::ptr::null_mut()
        }
    }
}

/// The error of the last `minigraf_fact_log_next_batch`, or NULL if it had
/// none. Valid until the next call on the log.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_fact_log_last_error(log: *mut MiniGrafFactLog) -> *const c_char {
    if log.is_null() {
        return std::ptr::null();
    }
    unsafe { &*log }.last_error.as_ptr()
}

/// Close a fact log, releasing the database, and free it.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_fact_log_free(log: *mut MiniGrafFactLog) {
    if !log.is_null() {
        unsafe { drop(Box::from_raw(log)) };
    }
}

// ─── Log writer ───────────────────────────────────────────────────────────────

pub struct MiniGrafLogWriter {
    /// `None` after `minigraf_log_writer_finish`.
    writer: Mutex<Option<minigraf::LogWriter>>,
    last_error: LastError,
}

impl MiniGrafLogWriter {
    /// Run `f` on the open writer, recording its error. 0 on success, -1 on error.
    fn call(&self, f: impl FnOnce(&mut minigraf::LogWriter) -> Result<(), MinigrafError>) -> c_int {
        let result = match self.writer.lock() {
            Ok(mut guard) => match guard.as_mut() {
                Some(w) => f(w).map_err(|e| e.to_string()),
                None => Err(MinigrafError::closed("log writer").to_string()),
            },
            Err(_) => Err("log writer lock poisoned".to_string()),
        };
        match result {
            Ok(()) => {
                self.last_error.clear();
                0
            }
            Err(e) => {
                self.last_error.set(e);
                -1
            }
        }
    }
}

/// Start building a new database at `path` from fact records (`STG-043` if it
/// exists). `options_json` is as for `minigraf_open_with_options`. Returns NULL
/// on error and, if `error_out` is not NULL, stores the message there (free it
/// with `minigraf_string_free`). Free the writer with
/// `minigraf_log_writer_free`; freeing it before `minigraf_log_writer_finish`
/// abandons the build and leaves no file.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_log_writer_create(
    path: *const c_char,
    options_json: *const c_char,
    error_out: *mut *mut c_char,
) -> *mut MiniGrafLogWriter {
    let created = arg_str(path, "path")
        .and_then(|path| minigraf::LogWriter::create(path, open_options(options_json)?));
    match created {
        Ok(w) => Box::into_raw(Box::new(MiniGrafLogWriter {
            writer: Mutex::new(Some(w)),
            last_error: LastError::default(),
        })),
        Err(e) => {
            report(error_out, e.to_string());
            std::ptr::null_mut()
        }
    }
}

/// Append one record, a JSON object as returned by
/// `minigraf_fact_log_next_batch`. A rejected record changes nothing.
/// Returns 0 on success, -1 on error (call `minigraf_log_writer_last_error`).
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_log_writer_append(
    writer: *mut MiniGrafLogWriter,
    record_json: *const c_char,
) -> c_int {
    if writer.is_null() {
        return -1;
    }
    let writer = unsafe { &*writer };
    writer.call(|w| {
        let record: FactRecordJson = serde_json::from_str(arg_str(record_json, "record_json")?)
            .map_err(|e| MinigrafError::invalid_argument(format!("record_json: {e}")))?;
        w.append(&record.to_core()?)
    })
}

/// Append a JSON array of records in order, stopping at the first rejected one,
/// whose error ends with `(batch index N)`; the records before it stay
/// appended. Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_log_writer_append_batch(
    writer: *mut MiniGrafLogWriter,
    records_json: *const c_char,
) -> c_int {
    if writer.is_null() {
        return -1;
    }
    let writer = unsafe { &*writer };
    let records: Vec<FactRecordJson> = match arg_str(records_json, "records_json").and_then(|s| {
        serde_json::from_str(s)
            .map_err(|e| MinigrafError::invalid_argument(format!("records_json: {e}")))
    }) {
        Ok(r) => r,
        Err(e) => {
            writer.last_error.set(e.to_string());
            return -1;
        }
    };
    let mut rejected = None;
    let rc = writer.call(|w| {
        for (i, record) in records.iter().enumerate() {
            if let Err(e) = record.to_core().and_then(|r| w.append(&r)) {
                rejected = Some((i, e.to_string()));
                return Err(e);
            }
        }
        Ok(())
    });
    if let Some((i, msg)) = rejected {
        writer.last_error.set(format!("{msg} (batch index {i})"));
    }
    rc
}

/// Close the open transaction and raise the counter to `tx_count`.
/// Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_log_writer_advance_tx_count(
    writer: *mut MiniGrafLogWriter,
    tx_count: u64,
) -> c_int {
    if writer.is_null() {
        return -1;
    }
    unsafe { &*writer }.call(|w| w.advance_tx_count(tx_count))
}

/// The highest `tx_count` appended or advanced to, in `*out`. Returns 0 on
/// success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_log_writer_tx_count(
    writer: *mut MiniGrafLogWriter,
    out: *mut u64,
) -> c_int {
    if writer.is_null() || out.is_null() {
        return -1;
    }
    unsafe { &*writer }.call(|w| {
        unsafe { *out = w.tx_count() };
        Ok(())
    })
}

/// Commit every record and rename the file into place. The writer must still
/// be freed; later calls fail with API-018. Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_log_writer_finish(writer: *mut MiniGrafLogWriter) -> c_int {
    if writer.is_null() {
        return -1;
    }
    let writer = unsafe { &*writer };
    let taken = match writer.writer.lock() {
        Ok(mut guard) => guard.take(),
        Err(_) => None,
    };
    let result = match taken {
        Some(w) => w.finish(),
        None => Err(MinigrafError::closed("log writer")),
    };
    match result {
        Ok(()) => {
            writer.last_error.clear();
            0
        }
        Err(e) => {
            writer.last_error.set(e.to_string());
            -1
        }
    }
}

/// The error of the last call on the writer, or NULL if it had none. Valid
/// until the next call on the writer.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_log_writer_last_error(writer: *mut MiniGrafLogWriter) -> *const c_char {
    if writer.is_null() {
        return std::ptr::null();
    }
    unsafe { &*writer }.last_error.as_ptr()
}

/// Free a writer. An unfinished build is abandoned and leaves no file.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_log_writer_free(writer: *mut MiniGrafLogWriter) {
    if !writer.is_null() {
        unsafe { drop(Box::from_raw(writer)) };
    }
}

// ─── Error ────────────────────────────────────────────────────────────────────

/// Return the last error message. Valid until the next call on the same handle.
/// Returns NULL if no error has occurred.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_last_error(handle: *mut MiniGrafDb) -> *const c_char {
    if handle.is_null() {
        return std::ptr::null();
    }
    let handle = unsafe { &*handle };
    let guard = match handle.last_error.lock() {
        Ok(g) => g,
        Err(_) => return std::ptr::null(),
    };
    match guard.as_ref() {
        Some(s) => s.as_ptr(),
        None => std::ptr::null(),
    }
}

// ─── JSON helpers ─────────────────────────────────────────────────────────────

fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::String(s) => J::String(s.clone()),
        Value::Integer(i) => serde_json::json!(i),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(J::Number)
            .unwrap_or(J::Null),
        Value::Boolean(b) => J::Bool(*b),
        Value::Ref(u) => J::String(u.to_string()),
        Value::Keyword(k) => J::String(k.clone()),
        Value::Null => J::Null,
    }
}

fn rows_to_json(rows: &[Vec<Value>]) -> String {
    let rows: Vec<Vec<serde_json::Value>> = rows
        .iter()
        .map(|r| r.iter().map(value_to_json).collect())
        .collect();
    serde_json::Value::from(rows).to_string()
}

fn query_result_to_json(result: QueryResult) -> String {
    let val = match result {
        QueryResult::Transacted(tx_id) => {
            serde_json::json!({"transacted": tx_id})
        }
        QueryResult::Retracted(tx_id) => {
            serde_json::json!({"retracted": tx_id})
        }
        QueryResult::Ok => serde_json::json!({"ok": true}),
        QueryResult::QueryResults { vars, results } => {
            let rows: Vec<Vec<serde_json::Value>> = results
                .iter()
                .map(|r| r.iter().map(value_to_json).collect())
                .collect();
            serde_json::json!({"variables": vars, "results": rows})
        }
    };
    val.to_string()
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_returns_non_null() {
        let db = minigraf_open_in_memory();
        assert!(!db.is_null());
        minigraf_close(db);
    }

    #[test]
    fn execute_transact_returns_json() {
        let db = minigraf_open_in_memory();
        let datalog = CString::new(r#"(transact [[:alice :name "Alice"]])"#).unwrap();
        let result = minigraf_execute(db, datalog.as_ptr());
        assert!(!result.is_null());
        let s = unsafe { CStr::from_ptr(result) }.to_str().unwrap();
        assert!(s.contains("transacted"), "expected transacted in: {s}");
        minigraf_string_free(result);
        minigraf_close(db);
    }

    #[test]
    fn execute_query_returns_results() {
        let db = minigraf_open_in_memory();
        let tx = CString::new(r#"(transact [[:alice :name "Alice"]])"#).unwrap();
        let r = minigraf_execute(db, tx.as_ptr());
        assert!(!r.is_null());
        minigraf_string_free(r);

        let q = CString::new("(query [:find ?n :where [?e :name ?n]])").unwrap();
        let result = minigraf_execute(db, q.as_ptr());
        assert!(!result.is_null());
        let s = unsafe { CStr::from_ptr(result) }.to_str().unwrap();
        assert!(s.contains("Alice"), "expected Alice in: {s}");
        minigraf_string_free(result);
        minigraf_close(db);
    }

    #[test]
    fn execute_invalid_datalog_returns_null_and_sets_error() {
        let db = minigraf_open_in_memory();
        let bad = CString::new("not valid datalog !!!").unwrap();
        let result = minigraf_execute(db, bad.as_ptr());
        assert!(result.is_null(), "expected NULL for invalid datalog");

        let err = minigraf_last_error(db);
        assert!(!err.is_null(), "expected non-NULL error");
        let msg = unsafe { CStr::from_ptr(err) }.to_str().unwrap();
        assert!(!msg.is_empty(), "expected non-empty error message");
        minigraf_close(db);
    }

    #[test]
    fn checkpoint_returns_zero_on_success() {
        let db = minigraf_open_in_memory();
        let rc = minigraf_checkpoint(db);
        assert_eq!(rc, 0);
        minigraf_close(db);
    }

    #[test]
    fn string_free_null_is_safe() {
        // Should not panic or crash
        minigraf_string_free(std::ptr::null_mut());
    }

    fn take(s: *mut c_char) -> Option<String> {
        if s.is_null() {
            return None;
        }
        let out = unsafe { CStr::from_ptr(s) }.to_str().unwrap().to_string();
        minigraf_string_free(s);
        Some(out)
    }

    fn text(p: *const c_char) -> String {
        assert!(!p.is_null(), "expected an error");
        unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string()
    }

    fn exec(db: *mut MiniGrafDb, q: &str) {
        let q = CString::new(q).unwrap();
        assert!(
            take(minigraf_execute(db, q.as_ptr())).is_some(),
            "execute failed"
        );
    }

    fn tmp(name: &str) -> (std::path::PathBuf, CString) {
        let dir =
            std::env::temp_dir().join(format!("minigraf_c_etl_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("db.graph");
        let c = CString::new(file.to_str().unwrap()).unwrap();
        (file, c)
    }

    fn read_log(db: *mut MiniGrafDb) -> Vec<serde_json::Value> {
        let log = minigraf_fact_log(db, std::ptr::null());
        assert!(!log.is_null());
        let mut out = Vec::new();
        while let Some(batch) = take(minigraf_fact_log_next_batch(log, 2)) {
            let batch: Vec<serde_json::Value> = serde_json::from_str(&batch).unwrap();
            assert!(!batch.is_empty() && batch.len() <= 2);
            out.extend(batch);
        }
        assert!(minigraf_fact_log_last_error(log).is_null());
        minigraf_fact_log_free(log);
        out
    }

    #[test]
    fn cursor_batches_match_execute_and_end() {
        let db = minigraf_open_in_memory();
        for i in 0..10 {
            exec(db, &format!("(transact [[:e{i} :n {i}]])"));
        }
        let q = CString::new("(query [:find ?n :where [?e :n ?n]])").unwrap();
        let all: serde_json::Value =
            serde_json::from_str(&take(minigraf_execute(db, q.as_ptr())).unwrap()).unwrap();
        let mut expected: Vec<i64> = all["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r[0].as_i64().unwrap())
            .collect();
        expected.sort_unstable();
        for size in [1, 7, 1000] {
            let cursor = minigraf_query(db, q.as_ptr());
            assert!(!cursor.is_null());
            assert_eq!(take(minigraf_cursor_vars(cursor)).unwrap(), r#"["?n"]"#);
            let mut got = Vec::new();
            while let Some(batch) = take(minigraf_cursor_next_batch(cursor, size)) {
                let rows: Vec<Vec<i64>> = serde_json::from_str(&batch).unwrap();
                assert!(!rows.is_empty() && rows.len() <= size);
                got.extend(rows.into_iter().map(|r| r[0]));
            }
            assert!(minigraf_cursor_last_error(cursor).is_null());
            got.sort_unstable();
            assert_eq!(got, expected);
            minigraf_cursor_free(cursor);
        }
        let tx = CString::new("(transact [[:a :n 1]])").unwrap();
        assert!(minigraf_query(db, tx.as_ptr()).is_null());
        assert!(text(minigraf_last_error(db)).starts_with("[API-012]"));
        minigraf_close(db);
    }

    #[test]
    fn read_only_open_and_errors() {
        let (file, path) = tmp("ro");
        let db = minigraf_open(path.as_ptr());
        exec(db, "(transact [[:a :n 1]])");
        minigraf_close(db);

        let ro_opts = CString::new(r#"{"read_only": true, "page_cache_size": 16}"#).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let ro = minigraf_open_with_options(path.as_ptr(), ro_opts.as_ptr(), &mut err);
        let ro2 = minigraf_open_with_options(path.as_ptr(), ro_opts.as_ptr(), &mut err);
        assert!(!ro.is_null() && !ro2.is_null() && err.is_null());
        let mut count = 0;
        assert_eq!(minigraf_current_tx_count(ro2, &mut count), 0);
        assert_eq!(count, 1);
        let tx = CString::new("(transact [[:b :n 2]])").unwrap();
        assert!(minigraf_execute(ro, tx.as_ptr()).is_null());
        assert!(text(minigraf_last_error(ro)).starts_with("[API-014]"));
        minigraf_close(ro);
        minigraf_close(ro2);

        let missing = CString::new(file.with_file_name("missing.graph").to_str().unwrap()).unwrap();
        assert!(minigraf_open_with_options(missing.as_ptr(), ro_opts.as_ptr(), &mut err).is_null());
        assert!(take(err).unwrap().starts_with("[STG-042]"));

        let bad = CString::new(r#"{"read_only": true, "bogus": 1}"#).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        assert!(minigraf_open_with_options(path.as_ptr(), bad.as_ptr(), &mut err).is_null());
        assert!(take(err).unwrap().starts_with("[API-017]"));
        // A NULL error_out is allowed.
        assert!(
            minigraf_open_with_options(missing.as_ptr(), ro_opts.as_ptr(), std::ptr::null_mut())
                .is_null()
        );
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn query_memory_limit_is_qry_012() {
        let (file, path) = tmp("qml");
        // A cartesian product holds one side in full.
        let product = CString::new("(query [:find ?a ?b :where [?a :n _] [?b :m _]])").unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();

        let small = CString::new(r#"{"query_memory_limit": 64}"#).unwrap();
        let db = minigraf_open_with_options(path.as_ptr(), small.as_ptr(), &mut err);
        assert!(!db.is_null() && err.is_null());
        exec(db, "(transact [[:a :n 1] [:b :m 2]])");
        assert!(minigraf_execute(db, product.as_ptr()).is_null());
        assert!(text(minigraf_last_error(db)).starts_with("[QRY-012]"));
        minigraf_close(db);

        // SIZE_MAX is no limit, the default.
        let max = CString::new(format!(r#"{{"query_memory_limit": {}}}"#, usize::MAX)).unwrap();
        let db = minigraf_open_with_options(path.as_ptr(), max.as_ptr(), &mut err);
        assert!(!db.is_null() && err.is_null());
        assert!(take(minigraf_execute(db, product.as_ptr())).is_some());
        minigraf_close(db);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn fact_log_to_log_writer_round_trip() {
        let (file, path) = tmp("rt");
        let src = minigraf_open(path.as_ptr());
        exec(
            src,
            r#"(transact [[:a :name "A"] [:a :friend #uuid "00000000-0000-4000-8000-000000000001"]])"#,
        );
        exec(src, "(transact [[:a :n 1]])");
        exec(src, "(retract [[:a :n 1]])");
        let records = read_log(src);
        assert_eq!(records.len(), 4);
        let friend = records
            .iter()
            .find(|r| r["attribute"] == ":friend")
            .unwrap();
        assert_eq!(friend["value"]["type"], "ref");
        assert_eq!(records[0]["valid_to"], serde_json::json!(i64::MAX));
        let filter = CString::new(r#"{"tx_from": 2, "tx_to": 2}"#).unwrap();
        let log = minigraf_fact_log(src, filter.as_ptr());
        let batch: Vec<serde_json::Value> =
            serde_json::from_str(&take(minigraf_fact_log_next_batch(log, 100)).unwrap()).unwrap();
        assert!(batch.iter().all(|r| r["tx_count"] == 2));
        minigraf_fact_log_free(log);

        let out = file.with_file_name("out.graph");
        let out_c = CString::new(out.to_str().unwrap()).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let w = minigraf_log_writer_create(out_c.as_ptr(), std::ptr::null(), &mut err);
        assert!(!w.is_null());
        let first = CString::new(records[0].to_string()).unwrap();
        assert_eq!(minigraf_log_writer_append(w, first.as_ptr()), 0);
        let rest =
            CString::new(serde_json::Value::from(records[1..].to_vec()).to_string()).unwrap();
        assert_eq!(minigraf_log_writer_append_batch(w, rest.as_ptr()), 0);
        let mut tx = 0;
        assert_eq!(minigraf_current_tx_count(src, &mut tx), 0);
        assert_eq!(minigraf_log_writer_advance_tx_count(w, tx), 0);
        let mut wtx = 0;
        assert_eq!(minigraf_log_writer_tx_count(w, &mut wtx), 0);
        assert_eq!(wtx, 3);
        assert_eq!(minigraf_log_writer_finish(w), 0);
        assert_eq!(minigraf_log_writer_finish(w), -1);
        assert!(text(minigraf_log_writer_last_error(w)).starts_with("[API-018]"));
        minigraf_log_writer_free(w);

        let ro = CString::new(r#"{"read_only": true}"#).unwrap();
        let copy = minigraf_open_with_options(out_c.as_ptr(), ro.as_ptr(), &mut err);
        assert!(!copy.is_null());
        let mut ctx = 0;
        assert_eq!(minigraf_current_tx_count(copy, &mut ctx), 0);
        assert_eq!(ctx, tx);
        assert_eq!(read_log(copy), records);
        minigraf_close(copy);

        // Errors: target exists, out of order with a batch index, bad UUID.
        assert!(minigraf_log_writer_create(out_c.as_ptr(), std::ptr::null(), &mut err).is_null());
        assert!(take(err).unwrap().starts_with("[STG-043]"));
        let abandoned = file.with_file_name("abandoned.graph");
        let ab_c = CString::new(abandoned.to_str().unwrap()).unwrap();
        let mut err: *mut c_char = std::ptr::null_mut();
        let w = minigraf_log_writer_create(ab_c.as_ptr(), std::ptr::null(), &mut err);
        let last = CString::new(records[3].to_string()).unwrap();
        assert_eq!(minigraf_log_writer_append(w, last.as_ptr()), 0);
        let disorder = CString::new(
            serde_json::Value::from(vec![records[3].clone(), records[0].clone()]).to_string(),
        )
        .unwrap();
        assert_eq!(minigraf_log_writer_append_batch(w, disorder.as_ptr()), -1);
        let msg = text(minigraf_log_writer_last_error(w));
        assert!(msg.starts_with("[API-015]") && msg.ends_with("(batch index 1)"));
        let mut bad = records[3].clone();
        bad["entity"] = "alice".into();
        bad["tx_count"] = 9.into();
        let bad = CString::new(bad.to_string()).unwrap();
        assert_eq!(minigraf_log_writer_append(w, bad.as_ptr()), -1);
        assert!(text(minigraf_log_writer_last_error(w)).starts_with("[API-017]"));
        minigraf_log_writer_free(w);
        assert!(!abandoned.exists());
        assert!(!abandoned.with_file_name("abandoned.graph.partial").exists());
        minigraf_close(src);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn close_null_is_safe() {
        minigraf_close(std::ptr::null_mut());
    }

    // ─── Prepared queries (minigraf#181) ─────────────────────────────────

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn prepared_err(pq: *mut MiniGrafPrepared) -> String {
        let e = minigraf_prepared_last_error(pq);
        assert!(!e.is_null(), "an error is set");
        unsafe { CStr::from_ptr(e) }.to_str().unwrap().to_string()
    }

    fn seeded() -> *mut MiniGrafDb {
        let db = minigraf_open_in_memory();
        let tx = c(
            r#"(transact [[:a :n 1] [:b :n 2] [:c :n 3] [:a :s "x"] [:a :f 2.5] [:a :w 2.0] [:a :t true] [:a :k :kw] [:a :z nil]])"#,
        );
        take(minigraf_execute(db, tx.as_ptr())).unwrap();
        db
    }

    fn results(json: &str) -> Vec<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        v["results"].as_array().unwrap().clone()
    }

    fn run(pq: *mut MiniGrafPrepared, bindings: &str) -> Vec<serde_json::Value> {
        let b = c(bindings);
        let out = take(minigraf_prepared_execute(pq, b.as_ptr()));
        results(&out.unwrap_or_else(|| panic!("execute failed: {}", prepared_err(pq))))
    }

    #[test]
    fn prepared_query_runs_again_with_new_bindings() {
        let db = seeded();
        let q = c("(query [:find ?x :where [?e :n $n] [?e :n ?x]])");
        let pq = minigraf_prepare(db, q.as_ptr());
        assert!(!pq.is_null());
        for n in 1..=3 {
            assert_eq!(
                run(pq, &format!(r#"{{"n": {n}}}"#)),
                vec![serde_json::json!([n])]
            );
        }
        assert!(run(pq, r#"{"n": 9}"#).is_empty());
        minigraf_prepared_free(pq);
        minigraf_close(db);
    }

    #[test]
    fn every_plain_and_typed_value_binds() {
        let db = seeded();
        let a = minigraf::EntityId::new_v5(&minigraf::EntityId::NAMESPACE_OID, b":a").to_string();
        for (q, binding) in [
            ("(query [:find ?e :where [?e :n $v]])", "1".to_string()),
            ("(query [:find ?e :where [?e :s $v]])", r#""x""#.to_string()),
            ("(query [:find ?e :where [?e :f $v]])", "2.5".to_string()),
            ("(query [:find ?e :where [?e :w $v]])", "2.0".to_string()),
            ("(query [:find ?e :where [?e :t $v]])", "true".to_string()),
            ("(query [:find ?e :where [?e :z $v]])", "null".to_string()),
            (
                "(query [:find ?e :where [?e :n $v]])",
                r#"{"type": "integer", "value": 1}"#.to_string(),
            ),
            (
                "(query [:find ?e :where [?e :w $v]])",
                r#"{"type": "float", "value": 2}"#.to_string(),
            ),
            (
                "(query [:find ?e :where [?e :k $v]])",
                r#"{"type": "keyword", "value": ":kw"}"#.to_string(),
            ),
            (
                "(query [:find ?e :where [?e :z $v]])",
                r#"{"type": "null"}"#.to_string(),
            ),
            (
                "(query [:find ?x :where [$v :n ?x]])",
                format!(r#"{{"type": "entity", "value": "{a}"}}"#),
            ),
            (
                "(query [:find ?x :where [$v :n ?x]])",
                format!(r#"{{"type": "ref", "value": "{a}"}}"#),
            ),
            (
                "(query [:find ?x :as-of $v :where [?e :n ?x]])",
                r#"{"type": "txCount", "value": 1}"#.to_string(),
            ),
            (
                "(query [:find ?x :as-of $v :where [?e :n ?x]])",
                r#"{"type": "timestamp", "value": 9007199254740991}"#.to_string(),
            ),
            (
                "(query [:find ?x :valid-at $v :where [:a :n ?x]])",
                r#"{"type": "anyValidTime"}"#.to_string(),
            ),
        ] {
            let qs = c(q);
            let pq = minigraf_prepare(db, qs.as_ptr());
            assert!(!pq.is_null(), "prepare");
            assert!(
                !run(pq, &format!(r#"{{"v": {binding}}}"#)).is_empty(),
                "{binding} binds in {q}"
            );
            minigraf_prepared_free(pq);
        }
        // A JSON integer is an integer: 2 does not match the stored float 2.0.
        let qs = c("(query [:find ?e :where [?e :w $v]])");
        let pq = minigraf_prepare(db, qs.as_ptr());
        assert!(run(pq, r#"{"v": 2}"#).is_empty());
        minigraf_prepared_free(pq);
        minigraf_close(db);
    }

    #[test]
    fn prepared_cursor_matches_execute() {
        let db = seeded();
        let q = c("(query [:find ?e ?n :where [?e :n ?n] [(>= ?n $min)]])");
        let pq = minigraf_prepare(db, q.as_ptr());
        let expected = run(pq, r#"{"min": 2}"#);
        let b = c(r#"{"min": 2}"#);
        let cursor = minigraf_prepared_query(pq, b.as_ptr());
        assert!(!cursor.is_null());
        let mut seen = 0;
        while let Some(batch) = take(minigraf_cursor_next_batch(cursor, 1)) {
            seen += serde_json::from_str::<Vec<serde_json::Value>>(&batch)
                .unwrap()
                .len();
        }
        assert_eq!(seen, expected.len());
        assert_eq!(seen, 2);
        minigraf_cursor_free(cursor);
        minigraf_prepared_free(pq);
        minigraf_close(db);
    }

    #[test]
    fn bad_bindings_and_bind_mistakes_are_reported() {
        let db = seeded();
        let q = c("(query [:find ?e :where [?e :n $n]])");
        let pq = minigraf_prepare(db, q.as_ptr());
        for (bindings, code) in [
            ("{}", "[API-022]"),
            (r#"{"n": {"type": "txCount", "value": 1}}"#, "[API-024]"),
            (r#"{"n": {"type": "txCount", "value": -1}}"#, "[API-017]"),
            (r#"{"n": {"type": "null", "value": 1}}"#, "[API-017]"),
            (
                r#"{"n": {"type": "entity", "value": "not-a-uuid"}}"#,
                "[API-017]",
            ),
            (r#"{"n": {"type": "nonsense"}}"#, "[API-017]"),
            (r#"{"n": [1]}"#, "[API-017]"),
            (r#"{"n": 9223372036854775808}"#, "[API-017]"),
            ("[1]", "[API-017]"),
            ("{not json", "[API-017]"),
        ] {
            let b = c(bindings);
            assert!(
                minigraf_prepared_execute(pq, b.as_ptr()).is_null(),
                "{bindings} is refused"
            );
            assert!(
                prepared_err(pq).starts_with(code),
                "{bindings} gives {code}"
            );
            assert!(
                minigraf_prepared_query(pq, b.as_ptr()).is_null(),
                "query refuses {bindings}"
            );
            assert!(
                prepared_err(pq).starts_with(code),
                "query: {bindings} gives {code}"
            );
        }
        // NULL bindings mean none.
        assert!(minigraf_prepared_execute(pq, std::ptr::null()).is_null());
        assert!(prepared_err(pq).starts_with("[API-022]"));
        // A success clears the error.
        run(pq, r#"{"n": 1}"#);
        assert!(minigraf_prepared_last_error(pq).is_null());

        let attr = c("(query [:find ?v :where [?e $a ?v]])");
        assert!(minigraf_prepare(db, attr.as_ptr()).is_null());
        let e = unsafe { CStr::from_ptr(minigraf_last_error(db)) }
            .to_str()
            .unwrap();
        assert!(e.starts_with("[API-023]"));
        minigraf_prepared_free(pq);
        minigraf_close(db);
    }

    #[test]
    fn close_releases_prepared_queries_so_the_path_reopens() {
        let dir = std::env::temp_dir().join(format!("minigraf_c_prepared_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = c(dir.join("p.graph").to_str().unwrap());
        let db = minigraf_open(path.as_ptr());
        let tx = c("(transact [[:a :n 1]])");
        take(minigraf_execute(db, tx.as_ptr())).unwrap();
        let q = c("(query [:find ?e :where [?e :n $n]])");
        let pq = minigraf_prepare(db, q.as_ptr());
        minigraf_close(db);

        let b = c(r#"{"n": 1}"#);
        assert!(minigraf_prepared_execute(pq, b.as_ptr()).is_null());
        assert!(prepared_err(pq).starts_with("[API-018]"));
        assert!(minigraf_prepared_query(pq, b.as_ptr()).is_null());
        assert!(prepared_err(pq).starts_with("[API-018]"));

        let again = minigraf_open(path.as_ptr());
        assert!(
            !again.is_null(),
            "the prepared query no longer holds the file"
        );
        minigraf_close(again);
        minigraf_prepared_free(pq);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
