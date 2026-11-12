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
use std::ffi::{CStr, CString, c_void};
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
    /// so that none keeps the file open. Shared with the prepared queries,
    /// which list the queries their setters derive in it.
    prepared: Arc<Registry>,
    /// Every write transaction begun on this handle, rolled back when it is
    /// closed (minigraf#824).
    transactions: Arc<TxRegistry>,
}

/// The prepared queries of one handle, by weak reference; `None` once the
/// handle is closed, so that no query is listed after it.
/// cbindgen:ignore
type Registry = Mutex<Option<Vec<Weak<PreparedSlot>>>>;

fn new_registry() -> Arc<Registry> {
    Arc::new(Mutex::new(Some(Vec::new())))
}

impl Drop for MiniGrafDb {
    fn drop(&mut self) {
        // An open transaction holds the write lock and the file: roll it
        // back first. Its handle stays valid and reports API-018.
        let open = match self.transactions.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };
        for slot in open.into_iter().flatten().filter_map(|w| w.upgrade()) {
            if let Ok(mut tx) = slot.lock()
                && let Some(tx) = tx.take()
            {
                tx.rollback();
            }
        }
        // A prepared query holds the file open too (core's PreparedQuery keeps
        // the storage), so release every one before the database itself.
        let listed = match self.prepared.lock() {
            Ok(mut guard) => guard.take(),
            Err(_) => None,
        };
        for slot in listed.into_iter().flatten().filter_map(|w| w.upgrade()) {
            if let Ok(mut q) = slot.lock() {
                *q = None;
            }
        }
    }
}

/// List `query` in `registry` and return its slot; `API-018` if the handle
/// is closed.
fn register(
    registry: &Registry,
    query: minigraf::PreparedQuery,
) -> Result<Arc<PreparedSlot>, MinigrafError> {
    let mut guard = registry
        .lock()
        .map_err(|_| MinigrafError::invalid_argument("database lock poisoned"))?;
    let listed = guard
        .as_mut()
        .ok_or_else(|| MinigrafError::closed("database"))?;
    let slot = Arc::new(Mutex::new(Some(query)));
    listed.retain(|w| w.strong_count() > 0);
    listed.push(Arc::downgrade(&slot));
    Ok(slot)
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

    /// The database, cloned so that no lock of this handle is held while the
    /// core runs: a user function may call back into the same handle.
    fn core(&self) -> Result<minigraf::Minigraf, String> {
        self.db
            .lock()
            .map(|db| db.clone())
            .map_err(|_| "database lock poisoned".to_string())
    }

    /// Record the outcome of an `int` call: 0 on success, -1 on error.
    fn status(&self, result: Result<(), String>) -> c_int {
        match result {
            Ok(()) => {
                self.clear_error();
                0
            }
            Err(e) => {
                self.set_error(e);
                -1
            }
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
            prepared: new_registry(),
            transactions: new_tx_registry(),
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
            prepared: new_registry(),
            transactions: new_tx_registry(),
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

    let db = match handle.core() {
        Ok(db) => db,
        Err(e) => {
            handle.set_error(e);
            return std::ptr::null_mut();
        }
    };
    match db.execute(datalog) {
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
    let result = handle
        .core()
        .and_then(|db| db.checkpoint().map_err(|e| format!("{e:#}")));
    handle.status(result.map(drop))
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
    allow_overrides: Option<Vec<OverridableJson>>,
    transaction_chunk_facts: Option<usize>,
    max_iterations: Option<usize>,
}

/// A name in `"allow_overrides"`.
#[derive(Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum OverridableJson {
    MaxDerivedFacts,
    MaxResults,
    QueryMemoryLimit,
    MaxIterations,
}

/// Each `OverridableJson` with the core variant it stands for.
const OVERRIDABLE: [(OverridableJson, minigraf::Overridable); 4] = [
    (
        OverridableJson::MaxDerivedFacts,
        minigraf::Overridable::MaxDerivedFacts,
    ),
    (
        OverridableJson::MaxResults,
        minigraf::Overridable::MaxResults,
    ),
    (
        OverridableJson::QueryMemoryLimit,
        minigraf::Overridable::QueryMemoryLimit,
    ),
    (
        OverridableJson::MaxIterations,
        minigraf::Overridable::MaxIterations,
    ),
];

impl OverridableJson {
    fn to_core(self) -> minigraf::Overridable {
        OVERRIDABLE
            .iter()
            .find(|(j, _)| *j == self)
            .map(|(_, o)| *o)
            .unwrap_or(minigraf::Overridable::MaxDerivedFacts)
    }
}

/// The override list that applies when `"allow_overrides"` is absent, the
/// Rust `Overridable::DEFAULTS`, as a JSON array of names such as
/// `["max_derived_facts","max_results","max_iterations"]`. Free it with
/// `minigraf_string_free`. Returns NULL only if out of memory.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_overridable_defaults() -> *mut c_char {
    let names: Vec<OverridableJson> = minigraf::Overridable::DEFAULTS
        .iter()
        .filter_map(|d| OVERRIDABLE.iter().find(|(_, o)| o == d).map(|(j, _)| *j))
        .collect();
    to_c_json(&names)
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
        if let Some(list) = &self.allow_overrides {
            o = o.allow_overrides(list.iter().map(|v| v.to_core()));
        }
        if let Some(v) = self.transaction_chunk_facts {
            o = o.transaction_chunk_facts(v);
        }
        if let Some(v) = self.max_iterations {
            o = o.max_iterations(v);
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
/// `max_derived_facts`), `synchronous` (`"full"` or `"normal"`),
/// `query_memory_limit` (the most bytes one query may hold, else `QRY-012`;
/// default no limit), `allow_overrides` (an array of `"max_derived_facts"`,
/// `"max_results"`, `"query_memory_limit"` and `"max_iterations"`: the query
/// keywords query text may use; absent, `minigraf_overridable_defaults()`;
/// `[]` refuses them all with `API-025`), `transaction_chunk_facts` and
/// `max_iterations` (rounds per stratum of a rule query, else `INT-020`);
/// NULL means all defaults. An unknown key or name is `API-017`. Returns
/// NULL on error and, if
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
            prepared: new_registry(),
            transactions: new_tx_registry(),
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
    let result = handle
        .core()
        .map(|db| unsafe { *out = db.current_tx_count() });
    handle.status(result)
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
        .and_then(|q| {
            handle
                .core()
                .and_then(|db| db.query(q).map_err(|e| e.to_string()))
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
    /// Its database's registry, where the setters list the queries they derive.
    registry: Arc<Registry>,
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
        .and_then(|q| {
            handle
                .core()
                .and_then(|db| db.prepare(q).map_err(|e| e.to_string()))
        })
        .and_then(|q| register(&handle.prepared, q).map_err(|e| e.to_string()));
    match result {
        Ok(slot) => {
            handle.clear_error();
            Box::into_raw(Box::new(MiniGrafPrepared {
                slot,
                registry: handle.prepared.clone(),
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
/// "value": n}`, `{"type": "anyValidTime"}`, or `{"type": "set", "value":
/// [...]}`, a set of plain values and typed value objects (minigraf#316; a set
/// inside a set is API-021). Returns the result as JSON,
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

/// A new prepared query that may derive at most `n` facts; `prepared` is
/// unchanged. An allowed `:max-derived-facts` or `:max-results` in the query
/// still overrides it. `SIZE_MAX` means no limit. Not gated by
/// `allow_overrides`. Returns NULL on error (call
/// `minigraf_prepared_last_error` on `prepared`): API-018 after
/// `minigraf_close` of its database. The new query is freed with
/// `minigraf_prepared_free`, independently of `prepared`, and closed with its
/// database as `prepared` is.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_prepared_with_max_derived_facts(
    prepared: *mut MiniGrafPrepared,
    n: usize,
) -> *mut MiniGrafPrepared {
    derive(prepared, |q| q.with_max_derived_facts(n))
}

/// A new prepared query that may hold at most `bytes` bytes (QRY-012
/// beyond); `prepared` is unchanged. `0` lets only queries that hold no state
/// run. Otherwise as `minigraf_prepared_with_max_derived_facts`.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_prepared_with_query_memory_limit(
    prepared: *mut MiniGrafPrepared,
    bytes: usize,
) -> *mut MiniGrafPrepared {
    derive(prepared, |q| q.with_query_memory_limit(bytes))
}

/// A new prepared query whose rule strata may run at most `n` rounds
/// (INT-020 beyond); `prepared` is unchanged. An allowed `:max-iterations` in
/// the query still overrides it. Otherwise as
/// `minigraf_prepared_with_max_derived_facts`.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_prepared_with_max_iterations(
    prepared: *mut MiniGrafPrepared,
    n: usize,
) -> *mut MiniGrafPrepared {
    derive(prepared, |q| q.with_max_iterations(n))
}

/// A copy of `prepared` with `set` applied, listed under the same database.
fn derive(
    prepared: *mut MiniGrafPrepared,
    set: impl FnOnce(minigraf::PreparedQuery) -> minigraf::PreparedQuery,
) -> *mut MiniGrafPrepared {
    if prepared.is_null() {
        return std::ptr::null_mut();
    }
    let prepared = unsafe { &*prepared };
    prepared.last_error.clear();
    let copy = prepared
        .slot
        .lock()
        .map_err(|_| MinigrafError::invalid_argument("prepared query lock poisoned"))
        .and_then(|guard| {
            guard
                .clone()
                .ok_or_else(|| MinigrafError::closed("database"))
        });
    match copy.and_then(|q| register(&prepared.registry, set(q))) {
        Ok(slot) => Box::into_raw(Box::new(MiniGrafPrepared {
            slot,
            registry: prepared.registry.clone(),
            last_error: LastError::default(),
        })),
        Err(e) => {
            prepared.last_error.set(e.to_string());
            std::ptr::null_mut()
        }
    }
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
            Some(kind) => typed_value(
                name,
                &format!("{what} of type {kind:?}"),
                kind,
                o.get("value"),
            )?,
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
    name: &str,
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
        "txCount" => match value {
            Some(J::Number(n)) => match n.as_u64() {
                Some(u) => B::TxCount(u),
                None if n.as_i64().is_some() => {
                    return Err(MinigrafError::invalid_argument(format!(
                        "{what}: {n} is negative"
                    )));
                }
                None => return Err(mismatch()),
            },
            _ => return Err(mismatch()),
        },
        "timestamp" => B::Timestamp(integer()?),
        "set" => match value {
            Some(J::Array(elements)) => {
                let values = elements
                    .iter()
                    .enumerate()
                    .map(|(i, e)| match bind_value(&format!("{name}[{i}]"), e)? {
                        B::Val(v) => Ok(v),
                        _ => Err(MinigrafError::invalid_argument(format!(
                            "{what}: element {i} is not a value"
                        ))),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                B::Val(Value::Set(minigraf::ValueSet::new(values)?))
            }
            _ => return Err(mismatch()),
        },
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
/// `string`, `integer`, `float`, `boolean`, `ref` (a UUID string), `keyword`,
/// `null` (no `value`) and `set` (`value` an array of typed values, none a set,
/// in canonical order; minigraf#316).
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
    Set(Vec<ValueJson>),
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
            Value::Set(set) => ValueJson::Set(set.iter().cloned().map(ValueJson::from).collect()),
            // A value kind newer than this binding: its debug form.
            other => ValueJson::String(format!("{other:?}")),
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
            ValueJson::Set(elements) => Value::Set(minigraf::ValueSet::new(
                elements
                    .iter()
                    .map(ValueJson::to_core)
                    .collect::<Result<Vec<_>, _>>()?,
            )?),
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
        .and_then(|filter| {
            handle
                .core()
                .and_then(|db| db.fact_log(&filter).map_err(|e| e.to_string()))
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

// ─── User functions ───────────────────────────────────────────────────────────

/// The result and error of one user-function call, set with
/// `minigraf_udf_result` and `minigraf_udf_error`. A `ctx` is valid only
/// during the call it was passed to.
#[derive(Default)]
pub struct MiniGrafUdfContext {
    result: Option<Vec<u8>>,
    error: Option<String>,
}

/// Frees `user_data`, or an accumulator.
#[allow(non_camel_case_types)]
pub type minigraf_destroy_fn = Option<unsafe extern "C" fn(user_data: *mut c_void)>;

/// A predicate: 1 true, 0 false, -1 failed (after `minigraf_udf_error`).
/// `value_json` is a typed value, `{"type", "value"}`, as in fact records.
#[allow(non_camel_case_types)]
pub type minigraf_predicate_fn = Option<
    unsafe extern "C" fn(
        ctx: *mut MiniGrafUdfContext,
        user_data: *mut c_void,
        value_json: *const c_char,
    ) -> c_int,
>;

/// A new accumulator, or NULL when failed.
#[allow(non_camel_case_types)]
pub type minigraf_acc_new_fn = Option<
    unsafe extern "C" fn(ctx: *mut MiniGrafUdfContext, user_data: *mut c_void) -> *mut c_void,
>;

/// Add one value to `acc`: 0 ok, -1 failed.
#[allow(non_camel_case_types)]
pub type minigraf_acc_step_fn = Option<
    unsafe extern "C" fn(
        ctx: *mut MiniGrafUdfContext,
        acc: *mut c_void,
        value_json: *const c_char,
    ) -> c_int,
>;

/// The aggregate of the `count` values stepped so far: 0 ok after
/// `minigraf_udf_result`, -1 failed.
#[allow(non_camel_case_types)]
pub type minigraf_acc_finish_fn = Option<
    unsafe extern "C" fn(ctx: *mut MiniGrafUdfContext, acc: *mut c_void, count: i64) -> c_int,
>;

/// Fail the current call with `message` (copied; NULL gives `user function
/// failed`). The call fails whatever the callback then returns.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_udf_error(ctx: *mut MiniGrafUdfContext, message: *const c_char) {
    if ctx.is_null() {
        return;
    }
    let message = if message.is_null() {
        "user function failed".to_string()
    } else {
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    };
    unsafe { &mut *ctx }.error = Some(message);
}

/// Set the result of the current `finish` call: a typed value,
/// `{"type", "value"}` (copied). NULL clears it.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_udf_result(ctx: *mut MiniGrafUdfContext, value_json: *const c_char) {
    if ctx.is_null() {
        return;
    }
    unsafe { &mut *ctx }.result =
        (!value_json.is_null()).then(|| unsafe { CStr::from_ptr(value_json) }.to_bytes().to_vec());
}

impl MiniGrafUdfContext {
    /// The call's outcome: the error it set, else a failure when `rc` is -1 or
    /// not one of `ok`.
    fn check(&self, rc: c_int, ok: &[c_int]) -> Result<c_int, minigraf::UdfError> {
        if let Some(msg) = &self.error {
            return Err(minigraf::UdfError::new(msg.clone()));
        }
        if rc == -1 {
            return Err(minigraf::UdfError::new("user function failed"));
        }
        if !ok.contains(&rc) {
            return Err(minigraf::UdfError::new(format!(
                "user function returned {rc}"
            )));
        }
        Ok(rc)
    }
}

/// `value` as the JSON a callback receives.
fn udf_value(value: &Value) -> CString {
    c_string(serde_json::to_string(&ValueJson::from(value.clone())).unwrap_or_default())
}

/// `user_data` and its destructor, run when the last function holding it drops.
struct UserData {
    ptr: *mut c_void,
    destroy: minigraf_destroy_fn,
}

// SAFETY: the caller's contract in the registration docs: the callbacks and
// `user_data` are safe to use from any thread that queries the database.
unsafe impl Send for UserData {}
unsafe impl Sync for UserData {}

impl UserData {
    /// The pointer. A method, so that a closure captures the whole `UserData`
    /// (`Send + Sync`), not the raw pointer field.
    fn ptr(&self) -> *mut c_void {
        self.ptr
    }
}

impl Drop for UserData {
    fn drop(&mut self) {
        if let Some(destroy) = self.destroy {
            unsafe { destroy(self.ptr) };
        }
    }
}

/// A required callback, or API-017 naming it.
fn required<T>(f: Option<T>, what: &str) -> Result<T, String> {
    f.ok_or_else(|| MinigrafError::invalid_argument(format!("{what} is NULL")).to_string())
}

/// Register predicate `name`, called in Datalog as `[(name ?v)]`. Returns 0 on
/// success, -1 on error (call `minigraf_last_error`): API-017 for a NULL
/// argument, INT-040 for a name that is taken. On an error `destroy` has
/// already run. A failing call stops the query with QRY-014.
///
/// `destroy(user_data)` runs, if not NULL, when the last reference to the
/// function drops: after `minigraf_close` and after every cursor, fact log
/// and prepared query from the database is freed. Until then `user_data` must
/// stay valid. A query sees the functions registered when it opened.
///
/// Threading: `f` runs on the thread that calls `minigraf_execute`, a cursor's
/// `minigraf_cursor_next_batch` or a prepared query, and queries on different
/// threads may call it at once: `f` and `user_data` must be safe to use from
/// any thread that queries the database. `destroy` runs on the thread that
/// drops the last reference. A callback may query, write to and register
/// functions on its own database, but must not close it.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_register_predicate(
    db: *mut MiniGrafDb,
    name: *const c_char,
    f: minigraf_predicate_fn,
    user_data: *mut c_void,
    destroy: minigraf_destroy_fn,
) -> c_int {
    let user_data = UserData {
        ptr: user_data,
        destroy,
    };
    if db.is_null() {
        return -1;
    }
    let handle = unsafe { &*db };
    let result = (|| {
        let name = arg_str(name, "name").map_err(|e| e.to_string())?;
        let f = required(f, "f")?;
        let core = handle.core()?;
        core.try_register_predicate(name, move |v| {
            let json = udf_value(v);
            let mut ctx = MiniGrafUdfContext::default();
            let rc = unsafe { f(&mut ctx, user_data.ptr(), json.as_ptr()) };
            ctx.check(rc, &[0, 1]).map(|rc| rc == 1)
        })
        .map_err(|e| e.to_string())
    })();
    handle.status(result)
}

/// An aggregate's callbacks and `user_data`, shared by its accumulators.
struct AggregateFns {
    init: unsafe extern "C" fn(*mut MiniGrafUdfContext, *mut c_void) -> *mut c_void,
    step: unsafe extern "C" fn(*mut MiniGrafUdfContext, *mut c_void, *const c_char) -> c_int,
    finish: unsafe extern "C" fn(*mut MiniGrafUdfContext, *mut c_void, i64) -> c_int,
    acc_free: minigraf_destroy_fn,
    user_data: UserData,
}

/// One accumulator, freed with `acc_free` when the query drops it.
struct Accumulator {
    ptr: *mut c_void,
    fns: Arc<AggregateFns>,
}

// SAFETY: as for `UserData`; one query uses an accumulator on one thread at a
// time.
unsafe impl Send for Accumulator {}

impl Drop for Accumulator {
    fn drop(&mut self) {
        if let Some(free) = self.fns.acc_free {
            unsafe { free(self.ptr) };
        }
    }
}

/// Register aggregate `name`, used as `(name ?v)` in `:find` or with `:over`.
/// `init` makes one accumulator per group or window partition, `step` adds a
/// value to it and `finish` sets the aggregate with `minigraf_udf_result`;
/// `acc_free(acc)`, if not NULL, frees it when the query drops it (after
/// `finish`, on an error, or at an early `minigraf_cursor_free`). An
/// aggregate steps a group's non-nil values and gets `nil` with none; a window
/// steps every value and calls `finish` once per set of peer rows.
///
/// Returns 0 on success, -1 on error, as `minigraf_register_predicate`, whose
/// rules for `user_data`, `destroy`, failing calls and threading apply.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub extern "C" fn minigraf_register_aggregate(
    db: *mut MiniGrafDb,
    name: *const c_char,
    init: minigraf_acc_new_fn,
    step: minigraf_acc_step_fn,
    finish: minigraf_acc_finish_fn,
    acc_free: minigraf_destroy_fn,
    user_data: *mut c_void,
    destroy: minigraf_destroy_fn,
) -> c_int {
    let user_data = UserData {
        ptr: user_data,
        destroy,
    };
    if db.is_null() {
        return -1;
    }
    let handle = unsafe { &*db };
    let result = (|| {
        let name = arg_str(name, "name").map_err(|e| e.to_string())?;
        let fns = Arc::new(AggregateFns {
            init: required(init, "init")?,
            step: required(step, "step")?,
            finish: required(finish, "finish")?,
            acc_free,
            user_data,
        });
        let core = handle.core()?;
        core.try_register_aggregate(
            name,
            move || {
                let mut ctx = MiniGrafUdfContext::default();
                let ptr = unsafe { (fns.init)(&mut ctx, fns.user_data.ptr()) };
                // Wrapped first, so that a failing init's accumulator is freed.
                let acc = (!ptr.is_null()).then(|| Accumulator {
                    ptr,
                    fns: Arc::clone(&fns),
                });
                ctx.check(if ptr.is_null() { -1 } else { 0 }, &[0])?;
                acc.ok_or_else(|| minigraf::UdfError::new("user function failed"))
            },
            |acc: &mut Accumulator, v| {
                let json = udf_value(v);
                let mut ctx = MiniGrafUdfContext::default();
                let rc = unsafe { (acc.fns.step)(&mut ctx, acc.ptr, json.as_ptr()) };
                ctx.check(rc, &[0]).map(drop)
            },
            |acc: &Accumulator, count| {
                let mut ctx = MiniGrafUdfContext::default();
                let count = i64::try_from(count).unwrap_or(i64::MAX);
                let rc = unsafe { (acc.fns.finish)(&mut ctx, acc.ptr, count) };
                ctx.check(rc, &[0])?;
                let json = ctx
                    .result
                    .ok_or_else(|| minigraf::UdfError::new("finish set no result"))?;
                serde_json::from_slice::<ValueJson>(&json)
                    .map_err(|e| {
                        minigraf::UdfError::new(format!("finish result is not a value: {e}"))
                    })?
                    .to_core()
                    .map_err(|e| minigraf::UdfError::new(e.to_string()))
            },
        )
        .map_err(|e| e.to_string())
    })();
    handle.status(result)
}

// ─── Write transactions (minigraf#824) ────────────────────────────────────────

/// An open transaction, `None` once finished.
/// cbindgen:ignore
type TxSlot = Mutex<Option<minigraf::OwnedWriteTransaction>>;

/// The transactions of one handle, by weak reference; `None` once the handle
/// is closed, so that no transaction is listed after it.
/// cbindgen:ignore
type TxRegistry = Mutex<Option<Vec<Weak<TxSlot>>>>;

fn new_tx_registry() -> Arc<TxRegistry> {
    Arc::new(Mutex::new(Some(Vec::new())))
}

/// `options_json` of `minigraf_begin_write`: every key optional, an absent
/// key is the database's value.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TxOptionsJson {
    synchronous: Option<SyncModeJson>,
    chunk_facts: Option<usize>,
}

impl TxOptionsJson {
    fn to_core(&self) -> minigraf::TxOptions {
        let mut o = minigraf::TxOptions::new();
        if let Some(v) = self.synchronous {
            o = o.synchronous(match v {
                SyncModeJson::Full => minigraf::SyncMode::Full,
                SyncModeJson::Normal => minigraf::SyncMode::Normal,
            });
        }
        if let Some(v) = self.chunk_facts {
            o = o.chunk_facts(v);
        }
        o
    }
}

/// A write transaction from `minigraf_begin_write`.
pub struct MiniGrafTx {
    slot: Arc<TxSlot>,
    last_error: LastError,
    /// The thread running a statement of this transaction, which holds
    /// `slot` meanwhile. A call from that thread comes from a user function
    /// of the statement and would wait for `slot` forever (minigraf#886).
    running: Mutex<Option<std::thread::ThreadId>>,
}

/// Clears `MiniGrafTx::running` when the statement ends, also by a panic.
struct Running<'a>(&'a Mutex<Option<std::thread::ThreadId>>);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }
}

/// INT-001: a user function called the transaction whose statement runs it.
fn tx_reentered() -> String {
    "[INT-001] a write transaction is already in progress on this thread; \
     a user function cannot call the transaction it runs in"
        .to_string()
}

/// API-018: a call on a finished transaction.
fn tx_finished() -> String {
    MinigrafError::closed("write transaction").to_string()
}

impl MiniGrafTx {
    /// Run `f` on the open transaction; on an error, set `last_error`.
    fn with<T>(
        &self,
        f: impl FnOnce(&mut minigraf::OwnedWriteTransaction) -> Result<T, MinigrafError>,
    ) -> Option<T> {
        if self.in_own_statement() {
            return self.outcome(Err(tx_reentered()));
        }
        let result = match self.slot.lock() {
            Ok(mut guard) => match guard.as_mut() {
                Some(tx) => {
                    // Declared after `guard`, so cleared before `slot` is
                    // released.
                    *self.running.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(std::thread::current().id());
                    let _running = Running(&self.running);
                    f(tx).map_err(|e| e.to_string())
                }
                None => Err(tx_finished()),
            },
            Err(_) => Err("transaction lock poisoned".to_string()),
        };
        self.outcome(result)
    }

    /// Whether this thread is running a statement of this transaction. Only
    /// the thread that set `running` can read its own id there, so the answer
    /// cannot change under the caller. Checked before `slot` is locked:
    /// another thread's call passes and waits for the statement.
    fn in_own_statement(&self) -> bool {
        *self.running.lock().unwrap_or_else(|e| e.into_inner()) == Some(std::thread::current().id())
    }

    fn outcome<T>(&self, result: Result<T, String>) -> Option<T> {
        match result {
            Ok(v) => {
                self.last_error.clear();
                Some(v)
            }
            Err(e) => {
                self.last_error.set(e);
                None
            }
        }
    }
}

/// Begin a write transaction on `handle`. `options_json` is a JSON object
/// with any of `synchronous` (`"full"` or `"normal"`: this commit's WAL sync)
/// and `chunk_facts` (facts staged before a file database writes the
/// transaction in chunks); NULL or an absent key is the database's value. An
/// unknown key or a negative count is `API-017`.
///
/// The transaction holds the database's write lock until it is committed,
/// rolled back or freed: a write or a begin from another thread waits until
/// then, and a query from another thread reads the committed state. On this
/// thread, the handle's calls that read or write (`minigraf_execute`,
/// `minigraf_query`, `minigraf_fact_log`, `minigraf_prepare`,
/// `minigraf_checkpoint`, `minigraf_begin_write`, a prepared query's runs)
/// fail with `INT-001` until it finishes. `API-014` on a read-only database.
///
/// Returns NULL on error (call `minigraf_last_error` on `handle`). Free the
/// transaction with `minigraf_tx_free`, also after `minigraf_close`.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_begin_write(
    handle: *mut MiniGrafDb,
    options_json: *const c_char,
) -> *mut MiniGrafTx {
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    let handle = unsafe { &*handle };
    let result = json_arg::<TxOptionsJson>(options_json, "options_json")
        .map_err(|e| e.to_string())
        .and_then(|options| {
            // No lock of this handle is held while the begin waits.
            handle.core().and_then(|db| {
                db.begin_write_owned_with(options.to_core())
                    .map_err(|e| e.to_string())
            })
        })
        .and_then(|tx| {
            let mut guard = handle
                .transactions
                .lock()
                .map_err(|_| "database lock poisoned".to_string())?;
            let Some(listed) = guard.as_mut() else {
                tx.rollback();
                return Err(MinigrafError::closed("database").to_string());
            };
            let slot = Arc::new(Mutex::new(Some(tx)));
            listed.retain(|w| w.strong_count() > 0);
            listed.push(Arc::downgrade(&slot));
            Ok(slot)
        });
    match result {
        Ok(slot) => {
            handle.clear_error();
            Box::into_raw(Box::new(MiniGrafTx {
                slot,
                last_error: LastError::default(),
                running: Mutex::new(None),
            }))
        }
        Err(e) => {
            handle.set_error(e);
            std::ptr::null_mut()
        }
    }
}

/// Run one Datalog command inside the transaction: a `transact`/`retract` is
/// staged and returns `{"ok":true}`, a `query` reads the committed facts plus
/// the staged ones, a `rule` is registered at once (a rollback keeps it).
/// Returns JSON encoded like `minigraf_execute`'s (free it with
/// `minigraf_string_free`), or NULL on error (call `minigraf_tx_last_error`).
/// A statement that fails stages nothing and leaves the transaction open.
/// `API-018` once the transaction is finished.
///
/// A user function cannot call the transaction whose statement runs it:
/// there `minigraf_tx_execute`, `minigraf_tx_commit` and
/// `minigraf_tx_rollback` fail with `INT-001` and change nothing, and
/// `minigraf_tx_free` must not be called.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_tx_execute(tx: *mut MiniGrafTx, datalog: *const c_char) -> *mut c_char {
    if tx.is_null() {
        return std::ptr::null_mut();
    }
    let tx = unsafe { &*tx };
    let datalog = match arg_str(datalog, "datalog") {
        Ok(d) => d,
        Err(e) => {
            tx.last_error.set(e.to_string());
            return std::ptr::null_mut();
        }
    };
    tx.with(|t| t.execute(datalog))
        .map_or(std::ptr::null_mut(), |r| {
            c_string(query_result_to_json(r)).into_raw()
        })
}

/// Commit every staged change as one transaction. Returns 0 on success, -1 on
/// error (call `minigraf_tx_last_error`). A commit that fails rolls the
/// transaction back; either way it is finished. `API-018` once finished.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_tx_commit(tx: *mut MiniGrafTx) -> c_int {
    if tx.is_null() {
        return -1;
    }
    let tx = unsafe { &*tx };
    if tx.in_own_statement() {
        return tx.outcome::<()>(Err(tx_reentered())).map_or(-1, |()| 0);
    }
    let result = match tx.slot.lock() {
        Ok(mut guard) => match guard.take() {
            Some(t) => t.commit().map_err(|e| e.to_string()),
            None => Err(tx_finished()),
        },
        Err(_) => Err("transaction lock poisoned".to_string()),
    };
    tx.outcome(result).map_or(-1, |()| 0)
}

/// Discard every staged change and finish the transaction. Does nothing on a
/// finished one. Returns 0; -1 for NULL, and with `INT-001` (call
/// `minigraf_tx_last_error`) from a user function of one of its statements.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_tx_rollback(tx: *mut MiniGrafTx) -> c_int {
    if tx.is_null() {
        return -1;
    }
    let tx = unsafe { &*tx };
    if tx.in_own_statement() {
        tx.last_error.set(tx_reentered());
        return -1;
    }
    if let Ok(mut guard) = tx.slot.lock()
        && let Some(t) = guard.take()
    {
        t.rollback();
    }
    tx.last_error.clear();
    0
}

/// The error of the last call on `tx`, or NULL if it had none. Valid until
/// the next call on it.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_tx_last_error(tx: *mut MiniGrafTx) -> *const c_char {
    if tx.is_null() {
        return std::ptr::null();
    }
    unsafe { &*tx }.last_error.as_ptr()
}

/// Free a transaction, rolling it back if it is open. Call it also after
/// `minigraf_close` of its database, which rolls back an open transaction.
/// NULL is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn minigraf_tx_free(tx: *mut MiniGrafTx) {
    if !tx.is_null() {
        unsafe { drop(Box::from_raw(tx)) };
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
        Value::Set(set) => J::Array(set.iter().map(value_to_json).collect()),
        // A value kind newer than this binding: its debug form.
        other => J::String(format!("{other:?}")),
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

        for (text, code) in [
            ("(query [:find ?v :where [?e $a ?v]])", "[API-023]"),
            ("(transact [[:a :n 1]])", "[API-005]"),
            ("(retract [[:a :n 1]])", "[API-006]"),
            ("(rule [(r ?x) [?x :n _]])", "[API-007]"),
        ] {
            let t = c(text);
            assert!(
                minigraf_prepare(db, t.as_ptr()).is_null(),
                "{text} is not preparable"
            );
            let e = unsafe { CStr::from_ptr(minigraf_last_error(db)) }
                .to_str()
                .unwrap();
            assert!(e.starts_with(code), "{text} gives {code}");
        }
        // A tx count may use the whole u64 range.
        let as_of = c("(query [:find ?x :as-of $t :where [?e :n ?x]])");
        let aq = minigraf_prepare(db, as_of.as_ptr());
        assert!(
            !run(
                aq,
                r#"{"t": {"type": "txCount", "value": 18446744073709551615}}"#
            )
            .is_empty()
        );
        minigraf_prepared_free(aq);
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

    // ─── Sets (minigraf#316) ─────────────────────────────────────────────

    #[test]
    fn sets_bind_and_come_back_as_arrays() {
        let db = seeded();
        exec(db, r#"(transact [[:a :tags #{1 :k "x"}]])"#);
        let q = c("(query [:find ?e :where [?e :n ?n] [(in? ?n $s)]])");
        let member = minigraf_prepare(db, q.as_ptr());
        for (b, want) in [
            (r#"{"s": {"type": "set", "value": [1, 3]}}"#, 2),
            (r#"{"s": {"type": "set", "value": [2, 2]}}"#, 1),
            (r#"{"s": {"type": "set", "value": []}}"#, 0),
        ] {
            assert_eq!(run(member, b).len(), want, "{b}");
        }
        let q = c("(query [:find ?x :where [?e :tags $s] [?e :n ?x]])");
        let equal = minigraf_prepare(db, q.as_ptr());
        let tags =
            r#"{"s": {"type": "set", "value": [{"type": "keyword", "value": ":k"}, 1, "x"]}}"#;
        assert_eq!(run(equal, tags), vec![serde_json::json!([1])]);
        assert!(
            run(equal, r#"{"s": {"type": "set", "value": [":k", 1, "x"]}}"#).is_empty(),
            "a string is no keyword"
        );
        let q = c("(query [:find ?t :where [:a :tags ?t]])");
        let out = take(minigraf_execute(db, q.as_ptr())).unwrap();
        assert_eq!(results(&out), vec![serde_json::json!([["x", 1, ":k"]])]);
        minigraf_prepared_free(member);
        minigraf_prepared_free(equal);
        minigraf_close(db);
    }

    #[test]
    fn bad_sets_are_rejected() {
        let db = seeded();
        let q = c("(query [:find ?e :where [?e :n ?n] [(in? ?n $s)]])");
        let pq = minigraf_prepare(db, q.as_ptr());
        let a = minigraf::EntityId::new_v5(&minigraf::EntityId::NAMESPACE_OID, b":a").to_string();
        let entity = format!(
            r#"{{"s": {{"type": "set", "value": [{{"type": "entity", "value": "{a}"}}]}}}}"#
        );
        for (bindings, code) in [
            (
                r#"{"s": {"type": "set", "value": [{"type": "set", "value": [1]}]}}"#,
                "[API-021]",
            ),
            (entity.as_str(), "[API-017]"),
            (
                r#"{"s": {"type": "set", "value": [{"type": "anyValidTime"}]}}"#,
                "[API-017]",
            ),
            (r#"{"s": {"type": "set", "value": [[1]]}}"#, "[API-017]"),
            (r#"{"s": {"type": "set", "value": 1}}"#, "[API-017]"),
            (r#"{"s": {"type": "set"}}"#, "[API-017]"),
            (r#"{"s": [1]}"#, "[API-017]"),
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
        }
        minigraf_prepared_free(pq);
        minigraf_close(db);
    }

    #[test]
    fn set_value_json_round_trips() {
        let id = minigraf::EntityId::new_v5(&minigraf::EntityId::NAMESPACE_OID, b":a");
        let set = minigraf::ValueSet::new([
            Value::Integer(1),
            Value::Keyword(":k".into()),
            Value::Ref(id),
        ])
        .unwrap();
        let json = serde_json::to_value(ValueJson::from(Value::Set(set.clone()))).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"type": "set", "value": [
                {"type": "integer", "value": 1},
                {"type": "ref", "value": id.to_string()},
                {"type": "keyword", "value": ":k"},
            ]})
        );
        let back: ValueJson = serde_json::from_value(json).unwrap();
        assert!(
            back.to_core().unwrap() == Value::Set(set),
            "set changed in round trip"
        );
        let nested: ValueJson = serde_json::from_value(
            serde_json::json!({"type": "set", "value": [{"type": "set", "value": []}]}),
        )
        .unwrap();
        assert_eq!(nested.to_core().err().unwrap().code(), "API-021");
    }

    // ─── User functions (minigraf#180) ───────────────────────────────────

    use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering::SeqCst};

    /// What the test callbacks saw, passed as `user_data`. Leaked, so that it
    /// outlives every database.
    #[derive(Default)]
    struct Counts {
        calls: AtomicUsize,
        destroyed: AtomicUsize,
        accs: AtomicUsize,
        freed: AtomicUsize,
        /// The database a re-entrant callback calls back into.
        db: AtomicPtr<MiniGrafDb>,
    }

    fn counts() -> &'static Counts {
        Box::leak(Box::default())
    }

    fn ud(counts: &'static Counts) -> *mut c_void {
        std::ptr::from_ref(counts).cast_mut().cast()
    }

    fn counts_of(user_data: *mut c_void) -> &'static Counts {
        unsafe { &*user_data.cast::<Counts>() }
    }

    fn value_of(value_json: *const c_char) -> serde_json::Value {
        serde_json::from_str(unsafe { CStr::from_ptr(value_json) }.to_str().unwrap()).unwrap()
    }

    unsafe extern "C" fn destroy(user_data: *mut c_void) {
        counts_of(user_data).destroyed.fetch_add(1, SeqCst);
    }

    /// `even?`: 1 for an even integer, 0 for an odd one, -1 with a message for
    /// anything else.
    unsafe extern "C" fn is_even(
        ctx: *mut MiniGrafUdfContext,
        user_data: *mut c_void,
        value_json: *const c_char,
    ) -> c_int {
        counts_of(user_data).calls.fetch_add(1, SeqCst);
        let v = value_of(value_json);
        match v["value"].as_i64() {
            Some(n) if v["type"] == "integer" => c_int::from(n % 2 == 0),
            _ => {
                minigraf_udf_error(ctx, c("not an integer").as_ptr());
                -1
            }
        }
    }

    unsafe extern "C" fn fails_silently(
        _ctx: *mut MiniGrafUdfContext,
        _user_data: *mut c_void,
        _value_json: *const c_char,
    ) -> c_int {
        -1
    }

    unsafe extern "C" fn errors_but_returns_true(
        ctx: *mut MiniGrafUdfContext,
        _user_data: *mut c_void,
        _value_json: *const c_char,
    ) -> c_int {
        minigraf_udf_error(ctx, c("set an error").as_ptr());
        1
    }

    unsafe extern "C" fn returns_two(
        _ctx: *mut MiniGrafUdfContext,
        _user_data: *mut c_void,
        _value_json: *const c_char,
    ) -> c_int {
        2
    }

    /// True when `:a` has `:n 1`, read through the database in `user_data`
    /// with both a cursor and `minigraf_execute`.
    unsafe extern "C" fn queries_own_db(
        ctx: *mut MiniGrafUdfContext,
        user_data: *mut c_void,
        _value_json: *const c_char,
    ) -> c_int {
        let db = counts_of(user_data).db.load(SeqCst);
        let q = c("(query [:find ?e :where [?e :n 1]])");
        let cursor = minigraf_query(db, q.as_ptr());
        if cursor.is_null() {
            minigraf_udf_error(ctx, c("minigraf_query failed").as_ptr());
            return -1;
        }
        let batch = take(minigraf_cursor_next_batch(cursor, 10));
        minigraf_cursor_free(cursor);
        let executed = take(minigraf_execute(db, q.as_ptr()));
        let other = c("(transact [[:x :seen true]])");
        let wrote = take(minigraf_execute(db, other.as_ptr())).is_some();
        let n = c("inner?");
        // Taken after the first call.
        let registered =
            minigraf_register_predicate(db, n.as_ptr(), Some(is_even), user_data, None) == 0
                || text(minigraf_last_error(db)).contains("INT-040");
        let found = batch.is_some_and(|b| b.len() > 2) && executed.is_some_and(|e| e.contains('['));
        c_int::from(found && wrote && registered)
    }

    /// `total`: the sum and the count, as `"sum/count"`.
    struct Total {
        sum: i64,
        counts: &'static Counts,
    }

    unsafe extern "C" fn total_new(
        _ctx: *mut MiniGrafUdfContext,
        user_data: *mut c_void,
    ) -> *mut c_void {
        let counts = counts_of(user_data);
        counts.accs.fetch_add(1, SeqCst);
        Box::into_raw(Box::new(Total { sum: 0, counts })).cast()
    }

    unsafe extern "C" fn new_fails(
        ctx: *mut MiniGrafUdfContext,
        _user_data: *mut c_void,
    ) -> *mut c_void {
        minigraf_udf_error(ctx, c("no accumulator").as_ptr());
        std::ptr::null_mut()
    }

    unsafe extern "C" fn total_step(
        _ctx: *mut MiniGrafUdfContext,
        acc: *mut c_void,
        value_json: *const c_char,
    ) -> c_int {
        let total = unsafe { &mut *acc.cast::<Total>() };
        total.sum += value_of(value_json)["value"].as_i64().unwrap();
        0
    }

    unsafe extern "C" fn step_fails(
        ctx: *mut MiniGrafUdfContext,
        _acc: *mut c_void,
        _value_json: *const c_char,
    ) -> c_int {
        minigraf_udf_error(ctx, c("bad step").as_ptr());
        -1
    }

    fn set_result(ctx: *mut MiniGrafUdfContext, json: &str) -> c_int {
        minigraf_udf_result(ctx, c(json).as_ptr());
        0
    }

    unsafe extern "C" fn total_finish(
        ctx: *mut MiniGrafUdfContext,
        acc: *mut c_void,
        count: i64,
    ) -> c_int {
        let total = unsafe { &*acc.cast::<Total>() };
        let sum = total.sum;
        set_result(
            ctx,
            &serde_json::json!({"type": "string", "value": format!("{sum}/{count}")}).to_string(),
        )
    }

    unsafe extern "C" fn finish_no_result(
        _ctx: *mut MiniGrafUdfContext,
        _acc: *mut c_void,
        _count: i64,
    ) -> c_int {
        0
    }

    unsafe extern "C" fn finish_bad_json(
        ctx: *mut MiniGrafUdfContext,
        _acc: *mut c_void,
        _count: i64,
    ) -> c_int {
        set_result(ctx, "{nope")
    }

    unsafe extern "C" fn finish_bad_ref(
        ctx: *mut MiniGrafUdfContext,
        _acc: *mut c_void,
        _count: i64,
    ) -> c_int {
        set_result(ctx, r#"{"type": "ref", "value": "nope"}"#)
    }

    unsafe extern "C" fn finish_fails_silently(
        _ctx: *mut MiniGrafUdfContext,
        _acc: *mut c_void,
        _count: i64,
    ) -> c_int {
        -1
    }

    unsafe extern "C" fn total_free(acc: *mut c_void) {
        let total = unsafe { Box::from_raw(acc.cast::<Total>()) };
        total.counts.freed.fetch_add(1, SeqCst);
    }

    fn register_even(
        db: *mut MiniGrafDb,
        name: &str,
        f: minigraf_predicate_fn,
        counts: &'static Counts,
    ) -> c_int {
        minigraf_register_predicate(db, c(name).as_ptr(), f, ud(counts), Some(destroy))
    }

    fn register_total(
        db: *mut MiniGrafDb,
        name: &str,
        init: minigraf_acc_new_fn,
        step: minigraf_acc_step_fn,
        finish: minigraf_acc_finish_fn,
        counts: &'static Counts,
    ) -> c_int {
        minigraf_register_aggregate(
            db,
            c(name).as_ptr(),
            init,
            step,
            finish,
            Some(total_free),
            ud(counts),
            Some(destroy),
        )
    }

    fn udf_db() -> *mut MiniGrafDb {
        let db = minigraf_open_in_memory();
        exec(
            db,
            "(transact [[:a :n 1] [:b :n 2] [:c :n 3] [:d :n 4] [:a :g 1] [:b :g 1] [:c :g 2] [:d :g 2]])",
        );
        db
    }

    fn sorted_rows(db: *mut MiniGrafDb, q: &str) -> Vec<serde_json::Value> {
        let out = take(minigraf_execute(db, c(q).as_ptr()))
            .unwrap_or_else(|| panic!("query failed: {}", text(minigraf_last_error(db))));
        let mut rows = results(&out);
        rows.sort_by_key(|r| r[0].as_i64());
        rows
    }

    fn query_err(db: *mut MiniGrafDb, q: &str) -> String {
        assert!(
            minigraf_execute(db, c(q).as_ptr()).is_null(),
            "query should fail"
        );
        text(minigraf_last_error(db))
    }

    #[test]
    fn predicate_filters_rows() {
        let db = udf_db();
        let counts = counts();
        assert_eq!(register_even(db, "even?", Some(is_even), counts), 0);
        let rows = sorted_rows(db, "(query [:find ?n :where [?e :n ?n] [(even? ?n)]])");
        assert_eq!(rows, vec![serde_json::json!([2]), serde_json::json!([4])]);
        assert_eq!(counts.calls.load(SeqCst), 4);
        assert_eq!(counts.destroyed.load(SeqCst), 0);
        minigraf_close(db);
        assert_eq!(counts.destroyed.load(SeqCst), 1);
    }

    #[test]
    fn failing_predicate_is_qry_014() {
        let db = udf_db();
        exec(db, r#"(transact [[:e :n "x"]])"#);
        assert_eq!(register_even(db, "even?", Some(is_even), counts()), 0);
        let msg = query_err(db, "(query [:find ?n :where [?e :n ?n] [(even? ?n)]])");
        assert!(msg.contains("QRY-014") && msg.contains("even?") && msg.contains("not an integer"));
        minigraf_close(db);
    }

    #[test]
    fn predicate_return_codes() {
        let db = udf_db();
        for (name, f) in [
            (
                "silent?",
                fails_silently as unsafe extern "C" fn(_, _, _) -> _,
            ),
            ("both?", errors_but_returns_true),
            ("two?", returns_two),
        ] {
            assert_eq!(register_even(db, name, Some(f), counts()), 0);
        }
        let q = |name: &str| format!("(query [:find ?n :where [?e :n ?n] [({name} ?n)]])");
        let msg = query_err(db, &q("silent?"));
        assert!(msg.contains("QRY-014") && msg.contains("user function failed"));
        // An error set with minigraf_udf_error fails the call, whatever it returns.
        let msg = query_err(db, &q("both?"));
        assert!(msg.contains("QRY-014") && msg.contains("set an error"));
        let msg = query_err(db, &q("two?"));
        assert!(msg.contains("QRY-014") && msg.contains("returned 2"));
        minigraf_close(db);
    }

    #[test]
    fn aggregate_in_find_and_over() {
        let db = udf_db();
        let counts = counts();
        let rc = register_total(
            db,
            "total",
            Some(total_new),
            Some(total_step),
            Some(total_finish),
            counts,
        );
        assert_eq!(rc, 0);
        let rows = sorted_rows(db, "(query [:find (total ?n) :where [?e :n ?n]])");
        assert_eq!(rows, vec![serde_json::json!(["10/4"])]);
        let rows = sorted_rows(
            db,
            "(query [:find ?n (total ?n :over (:partition-by ?g :order-by ?n)) :where [?e :n ?n] [?e :g ?g]])",
        );
        assert_eq!(
            rows,
            vec![
                serde_json::json!([1, "1/1"]),
                serde_json::json!([2, "3/2"]),
                serde_json::json!([3, "3/1"]),
                serde_json::json!([4, "7/2"]),
            ]
        );
        assert!(counts.accs.load(SeqCst) >= 3);
        assert_eq!(counts.freed.load(SeqCst), counts.accs.load(SeqCst));
        minigraf_close(db);
        assert_eq!(counts.destroyed.load(SeqCst), 1);
    }

    #[test]
    fn aggregate_failures_are_qry_014() {
        let db = udf_db();
        let counts = counts();
        let cases: [(
            &str,
            minigraf_acc_new_fn,
            minigraf_acc_step_fn,
            minigraf_acc_finish_fn,
            &str,
        ); 6] = [
            (
                "a",
                Some(new_fails),
                Some(total_step),
                Some(total_finish),
                "no accumulator",
            ),
            (
                "b",
                Some(total_new),
                Some(step_fails),
                Some(total_finish),
                "bad step",
            ),
            (
                "c",
                Some(total_new),
                Some(total_step),
                Some(finish_no_result),
                "no result",
            ),
            (
                "d",
                Some(total_new),
                Some(total_step),
                Some(finish_bad_json),
                "not a value",
            ),
            (
                "e",
                Some(total_new),
                Some(total_step),
                Some(finish_bad_ref),
                r#"ref value "nope" is not a UUID"#,
            ),
            (
                "f",
                Some(total_new),
                Some(total_step),
                Some(finish_fails_silently),
                "user function failed",
            ),
        ];
        for (name, init, step, finish, want) in cases {
            assert_eq!(register_total(db, name, init, step, finish, counts), 0);
            let msg = query_err(
                db,
                &format!("(query [:find ({name} ?n) :where [?e :n ?n]])"),
            );
            assert!(
                msg.contains("QRY-014") && msg.contains(want),
                "aggregate {name}: {msg}"
            );
        }
        assert_eq!(counts.freed.load(SeqCst), counts.accs.load(SeqCst));
        minigraf_close(db);
        assert_eq!(counts.destroyed.load(SeqCst), 6);
    }

    #[test]
    fn destroy_waits_for_every_cursor() {
        let db = udf_db();
        let counts = counts();
        assert_eq!(register_even(db, "even?", Some(is_even), counts), 0);
        let rc = register_total(
            db,
            "total",
            Some(total_new),
            Some(total_step),
            Some(total_finish),
            counts,
        );
        assert_eq!(rc, 0);
        let q = c("(query [:find ?n :where [?e :n ?n] [(even? ?n)]])");
        let filtered = minigraf_query(db, q.as_ptr());
        let q = c("(query [:find ?g (total ?n) :where [?e :n ?n] [?e :g ?g]])");
        let grouped = minigraf_query(db, q.as_ptr());
        assert!(!filtered.is_null() && !grouped.is_null());
        minigraf_close(db);
        assert_eq!(
            counts.destroyed.load(SeqCst),
            0,
            "a cursor still holds both functions"
        );
        assert!(take(minigraf_cursor_next_batch(filtered, 1)).is_some());
        // Freed before its first batch: every accumulator it made is freed.
        minigraf_cursor_free(grouped);
        assert_eq!(counts.freed.load(SeqCst), counts.accs.load(SeqCst));
        assert_eq!(
            counts.destroyed.load(SeqCst),
            0,
            "the other cursor still holds them"
        );
        minigraf_cursor_free(filtered);
        assert_eq!(counts.destroyed.load(SeqCst), 2);
    }

    #[test]
    fn failed_registration_runs_destroy() {
        let db = udf_db();
        let counts = counts();
        assert_eq!(register_even(db, "even?", Some(is_even), counts), 0);
        assert_eq!(register_even(db, "even?", Some(is_even), counts), -1);
        assert!(text(minigraf_last_error(db)).contains("INT-040"));
        assert_eq!(counts.destroyed.load(SeqCst), 1);
        assert_eq!(register_even(db, "none?", None, counts), -1);
        assert!(text(minigraf_last_error(db)).contains("API-017"));
        assert_eq!(
            register_total(db, "t", Some(total_new), None, Some(total_finish), counts),
            -1
        );
        assert!(text(minigraf_last_error(db)).contains("API-017"));
        let rc = minigraf_register_predicate(
            db,
            std::ptr::null(),
            Some(is_even),
            ud(counts),
            Some(destroy),
        );
        assert_eq!(rc, -1);
        assert!(text(minigraf_last_error(db)).contains("API-017"));
        assert_eq!(
            register_even(std::ptr::null_mut(), "x?", Some(is_even), counts),
            -1
        );
        assert_eq!(counts.destroyed.load(SeqCst), 5);
        minigraf_close(db);
        assert_eq!(counts.destroyed.load(SeqCst), 6);
    }

    #[test]
    fn callback_calls_back_into_its_database() {
        let db = udf_db();
        let counts = counts();
        counts.db.store(db, SeqCst);
        assert_eq!(register_even(db, "own?", Some(queries_own_db), counts), 0);
        let rows = sorted_rows(db, "(query [:find ?n :where [?e :n ?n] [(own? ?n)]])");
        assert_eq!(rows.len(), 4, "every call saw its own database");
        let seen = sorted_rows(db, "(query [:find ?e :where [?e :seen true]])");
        assert_eq!(seen.len(), 1, "the callbacks' writes landed");
        let rows = sorted_rows(db, "(query [:find ?n :where [?e :n ?n] [(inner? ?n)]])");
        assert_eq!(rows.len(), 2, "a function registered from a callback works");
        minigraf_close(db);
    }

    #[test]
    fn udf_context_null_arguments_are_safe() {
        minigraf_udf_error(std::ptr::null_mut(), c("x").as_ptr());
        minigraf_udf_result(std::ptr::null_mut(), c("1").as_ptr());
        let mut ctx = MiniGrafUdfContext::default();
        minigraf_udf_error(&mut ctx, std::ptr::null());
        minigraf_udf_result(&mut ctx, std::ptr::null());
        assert!(ctx.result.is_none());
        assert_eq!(ctx.error.as_deref(), Some("user function failed"));
    }

    // ─── Overrides, open options and prepared setters (minigraf#872) ─────

    const REACH: &str = "(query [:find ?x ?y :where (reach ?x ?y)])";
    const GROUPED: &str = "(query [:find ?v (count ?e) :where [?e :v ?v]])";

    /// Open `name` in a fresh directory with `options` and load a 4-edge
    /// chain (10 `reach` pairs) and 3 grouped values. Returns the handle, or
    /// the open error.
    fn reach_db(name: &str, options: &str) -> Result<*mut MiniGrafDb, String> {
        let (_, path) = tmp(name);
        let o = c(options);
        let mut err: *mut c_char = std::ptr::null_mut();
        let db = minigraf_open_with_options(path.as_ptr(), o.as_ptr(), &mut err);
        if db.is_null() {
            return Err(take(err).unwrap_or_default());
        }
        exec(
            db,
            "(transact [[:a :edge :b] [:b :edge :c] [:c :edge :d] [:d :edge :e] \
             [:p :v 1] [:q :v 1] [:r :v 2]])",
        );
        exec(db, "(rule [(reach ?x ?y) [?x :edge ?y]])");
        exec(db, "(rule [(reach ?x ?y) [?x :edge ?z] (reach ?z ?y)])");
        Ok(db)
    }

    /// The rows of `q`, or its error.
    fn rows_of(db: *mut MiniGrafDb, q: &str) -> Result<usize, String> {
        let q = c(q);
        match take(minigraf_execute(db, q.as_ptr())) {
            Some(json) => Ok(results(&json).len()),
            None => Err(text(minigraf_last_error(db))),
        }
    }

    fn prepared_rows(pq: *mut MiniGrafPrepared) -> Result<usize, String> {
        match take(minigraf_prepared_execute(pq, std::ptr::null())) {
            Some(json) => Ok(results(&json).len()),
            None => Err(prepared_err(pq)),
        }
    }

    fn prepare(db: *mut MiniGrafDb, q: &str) -> *mut MiniGrafPrepared {
        let q = c(q);
        let pq = minigraf_prepare(db, q.as_ptr());
        assert!(!pq.is_null(), "prepare failed");
        pq
    }

    #[test]
    fn overridable_defaults_are_the_core_defaults() {
        let json = take(minigraf_overridable_defaults()).unwrap();
        assert_eq!(
            json,
            r#"["max_derived_facts","max_results","max_iterations"]"#
        );
    }

    #[test]
    fn allow_overrides_replaces_the_defaults() {
        let derived = "(query [:find ?x ?y :where (reach ?x ?y) :max-derived-facts 100])";
        let memory = "(query [:find ?x ?y :where (reach ?x ?y) :query-memory-limit 1000000])";
        let defaults = take(minigraf_overridable_defaults()).unwrap();
        for (name, options, derived_ok, memory_ok) in [
            ("ov_absent", "{}".to_string(), true, false),
            ("ov_defaults", format!(r#"{{"allow_overrides": {defaults}}}"#), true, false),
            ("ov_empty", r#"{"allow_overrides": []}"#.into(), false, false),
            (
                "ov_memory",
                r#"{"allow_overrides": ["query_memory_limit", "query_memory_limit"]}"#.into(),
                false,
                true,
            ),
            (
                "ov_all",
                r#"{"allow_overrides": ["max_derived_facts", "max_results", "query_memory_limit", "max_iterations"]}"#.into(),
                true,
                true,
            ),
        ] {
            let db = reach_db(name, &options).expect("open");
            for (q, ok) in [(derived, derived_ok), (memory, memory_ok)] {
                let qc = c(q);
                if ok {
                    assert_eq!(rows_of(db, q), Ok(10), "{name}");
                    let cursor = minigraf_query(db, qc.as_ptr());
                    assert!(!cursor.is_null(), "{name} cursor");
                    minigraf_cursor_free(cursor);
                    minigraf_prepared_free(prepare(db, q));
                } else {
                    assert!(rows_of(db, q).unwrap_err().starts_with("[API-025]"), "{name}");
                    assert!(minigraf_query(db, qc.as_ptr()).is_null());
                    assert!(text(minigraf_last_error(db)).starts_with("[API-025]"));
                    assert!(minigraf_prepare(db, qc.as_ptr()).is_null());
                    assert!(text(minigraf_last_error(db)).starts_with("[API-025]"));
                }
            }
            minigraf_close(db);
        }
    }

    #[test]
    fn bad_override_names_and_counts_are_refused_like_synchronous() {
        for options in [
            r#"{"allow_overrides": ["max_rows"]}"#,
            r#"{"allow_overrides": "max_results"}"#,
            r#"{"transaction_chunk_facts": -1}"#,
            r#"{"max_iterations": -1}"#,
            r#"{"synchronous": "sometimes"}"#,
        ] {
            let e = reach_db("bad_options", options).expect_err("refused");
            assert!(e.starts_with("[API-017]"), "{options}");
        }
    }

    #[test]
    fn transaction_chunk_facts_commits_a_large_transaction() {
        let (file, path) = tmp("chunk");
        let o = c(r#"{"transaction_chunk_facts": 2}"#);
        let mut err: *mut c_char = std::ptr::null_mut();
        let db = minigraf_open_with_options(path.as_ptr(), o.as_ptr(), &mut err);
        assert!(!db.is_null());
        let facts: String = (0..10).map(|i| format!("[:e{i} :n {i}]")).collect();
        exec(db, &format!("(transact [{facts}])"));
        minigraf_close(db);
        let db = minigraf_open(path.as_ptr());
        assert!(!db.is_null());
        assert_eq!(rows_of(db, "(query [:find ?n :where [?e :n ?n]])"), Ok(10));
        minigraf_close(db);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn max_iterations_option_limits_rule_rounds() {
        let db = reach_db("it_default", "{}").unwrap();
        assert_eq!(rows_of(db, REACH), Ok(10));
        minigraf_close(db);
        let db = reach_db("it_one", r#"{"max_iterations": 1}"#).unwrap();
        assert!(rows_of(db, REACH).unwrap_err().starts_with("[INT-020]"));
        minigraf_close(db);
        let max = format!(r#"{{"max_iterations": {}}}"#, usize::MAX);
        let db = reach_db("it_max", &max).unwrap();
        assert_eq!(rows_of(db, REACH), Ok(10));
        minigraf_close(db);
    }

    #[test]
    fn log_writer_takes_the_new_options() {
        let (file, _) = tmp("writer_options");
        let out = c(file.with_file_name("out.graph").to_str().unwrap());
        let o = c(r#"{"allow_overrides": [], "transaction_chunk_facts": 2, "max_iterations": 5}"#);
        let mut err: *mut c_char = std::ptr::null_mut();
        let w = minigraf_log_writer_create(out.as_ptr(), o.as_ptr(), &mut err);
        assert!(!w.is_null());
        assert_eq!(minigraf_log_writer_finish(w), 0);
        minigraf_log_writer_free(w);
        assert!(file.with_file_name("out.graph").exists());
    }

    #[test]
    fn setters_derive_new_queries() {
        // The setters are not gated: every override keyword is refused here.
        let db = reach_db("setters", r#"{"allow_overrides": []}"#).unwrap();
        type Setter = extern "C" fn(*mut MiniGrafPrepared, usize) -> *mut MiniGrafPrepared;
        let cases: [(&str, Setter, usize, &str); 3] = [
            (
                REACH,
                minigraf_prepared_with_max_derived_facts,
                1,
                "[INT-020]",
            ),
            (REACH, minigraf_prepared_with_max_iterations, 1, "[INT-020]"),
            (
                GROUPED,
                minigraf_prepared_with_query_memory_limit,
                16,
                "[QRY-012]",
            ),
        ];
        for (q, setter, n, code) in cases {
            let pq = prepare(db, q);
            let before = prepared_rows(pq).unwrap();
            let cursor = minigraf_prepared_query(pq, std::ptr::null());
            let derived = setter(pq, n);
            assert!(!derived.is_null());
            assert!(prepared_rows(derived).unwrap_err().starts_with(code));
            // Deriving left the receiver's open cursor alone.
            let mut seen = 0;
            while let Some(batch) = take(minigraf_cursor_next_batch(cursor, 3)) {
                seen += serde_json::from_str::<Vec<serde_json::Value>>(&batch)
                    .unwrap()
                    .len();
            }
            assert_eq!(seen, before);
            minigraf_cursor_free(cursor);
            assert_eq!(prepared_rows(pq), Ok(before));
            let unlimited = setter(derived, usize::MAX);
            assert_eq!(prepared_rows(unlimited), Ok(before));
            // Each handle is freed on its own, in any order.
            minigraf_prepared_free(derived);
            assert_eq!(prepared_rows(unlimited), Ok(before));
            minigraf_prepared_free(pq);
            minigraf_prepared_free(unlimited);
        }

        // The last value for a limit applies; the others are kept.
        let pq = prepare(db, REACH);
        let a = minigraf_prepared_with_max_derived_facts(pq, 1);
        let b = minigraf_prepared_with_max_iterations(a, 1);
        let chained = minigraf_prepared_with_max_iterations(b, usize::MAX);
        assert!(prepared_rows(chained).unwrap_err().starts_with("[INT-020]"));
        let raised = minigraf_prepared_with_max_derived_facts(chained, usize::MAX);
        assert_eq!(prepared_rows(raised), Ok(10));

        // After minigraf_close, every handle, derived ones too, is API-018.
        minigraf_close(db);
        for h in [pq, a, chained, raised] {
            assert!(prepared_rows(h).unwrap_err().starts_with("[API-018]"));
        }
        assert!(minigraf_prepared_with_max_iterations(pq, 5).is_null());
        assert!(prepared_err(pq).starts_with("[API-018]"));
        for h in [pq, a, b, chained, raised] {
            minigraf_prepared_free(h);
        }
        assert!(minigraf_prepared_with_max_iterations(std::ptr::null_mut(), 1).is_null());
    }

    // ── Write transactions (minigraf#824) ────────────────────────────────────

    const NAMES: &str = "(query [:find ?n :where [?e :person/name ?n]])";
    const TX_SETUP: &str =
        r#"(transact [[:alice :person/name "Alice"] [:bob :person/name "Bob"]])"#;

    /// A raw pointer that may cross threads, as a C caller's would.
    struct Ptr<T>(*mut T);
    impl<T> Clone for Ptr<T> {
        fn clone(&self) -> Self {
            *self
        }
    }
    impl<T> Copy for Ptr<T> {}
    unsafe impl<T> Send for Ptr<T> {}
    unsafe impl<T> Sync for Ptr<T> {}

    impl<T> Ptr<T> {
        /// The pointer. A method, so that a closure captures the whole
        /// `Ptr` (which is `Send`) rather than its field.
        fn get(self) -> *mut T {
            self.0
        }
    }

    fn names_of(json: &str) -> Vec<String> {
        let mut names: Vec<String> = results(json)
            .iter()
            .map(|r| r[0].as_str().unwrap().to_string())
            .collect();
        names.sort();
        names
    }

    fn db_names(db: *mut MiniGrafDb) -> Vec<String> {
        let q = c(NAMES);
        names_of(&take(minigraf_execute(db, q.as_ptr())).expect("query"))
    }

    fn code_of(msg: &str) -> String {
        let start = msg.find('[').map_or(0, |i| i + 1);
        msg[start..].chars().take(7).collect()
    }

    fn db_code(db: *mut MiniGrafDb) -> String {
        code_of(&text(minigraf_last_error(db)))
    }

    fn tx_code(tx: *mut MiniGrafTx) -> String {
        code_of(&text(minigraf_tx_last_error(tx)))
    }

    fn tx_exec(tx: *mut MiniGrafTx, q: &str) -> Option<String> {
        let q = c(q);
        take(minigraf_tx_execute(tx, q.as_ptr()))
    }

    fn begin(db: *mut MiniGrafDb, options: Option<&str>) -> *mut MiniGrafTx {
        let o = options.map(c);
        minigraf_begin_write(db, o.as_ref().map_or(std::ptr::null(), |o| o.as_ptr()))
    }

    fn tx_db() -> *mut MiniGrafDb {
        let db = minigraf_open_in_memory();
        exec(db, TX_SETUP);
        db
    }

    fn within<T: Send + 'static>(
        t: std::time::Duration,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> (Option<T>, std::sync::mpsc::Receiver<T>) {
        let (send, recv) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = send.send(f());
        });
        match recv.recv_timeout(t) {
            Ok(v) => (Some(v), recv),
            Err(_) => (None, recv),
        }
    }

    const WAITS: std::time::Duration = std::time::Duration::from_millis(300);
    const STUCK: std::time::Duration = std::time::Duration::from_secs(30);

    #[test]
    fn tx_reads_its_own_writes_commits_and_survives_reopen() {
        let (file, path) = tmp("tx_commit");
        let db = minigraf_open(path.as_ptr());
        exec(db, TX_SETUP);
        let tx = begin(db, None);
        assert!(!tx.is_null());
        assert_eq!(
            tx_exec(tx, r#"(transact [[:carol :person/name "Carol"]])"#).as_deref(),
            Some(r#"{"ok":true}"#)
        );
        tx_exec(tx, r#"(retract [[:bob :person/name "Bob"]])"#).expect("retract");
        assert_eq!(
            names_of(&tx_exec(tx, NAMES).expect("query")),
            ["Alice", "Carol"]
        );
        let p = Ptr(db);
        let (outside, _) = within(STUCK, move || db_names(p.get()));
        assert_eq!(outside.expect("a query does not wait"), ["Alice", "Bob"]);
        assert_eq!(minigraf_tx_commit(tx), 0);
        minigraf_tx_free(tx);
        assert_eq!(db_names(db), ["Alice", "Carol"]);
        minigraf_close(db);
        let db = minigraf_open(path.as_ptr());
        assert_eq!(db_names(db), ["Alice", "Carol"], "after reopening");
        minigraf_close(db);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn tx_rollback_and_free_discard() {
        let (file, path) = tmp("tx_rollback");
        let db = minigraf_open(path.as_ptr());
        exec(db, TX_SETUP);
        let tx = begin(db, None);
        tx_exec(tx, r#"(transact [[:carol :person/name "Carol"]])"#).expect("transact");
        assert_eq!(minigraf_tx_rollback(tx), 0);
        minigraf_tx_free(tx);
        // Freed open: rolled back, and the write lock is free.
        let tx = begin(db, None);
        tx_exec(tx, r#"(transact [[:dave :person/name "Dave"]])"#).expect("transact");
        minigraf_tx_free(tx);
        assert_eq!(db_names(db), ["Alice", "Bob"]);
        exec(db, r#"(transact [[:erin :person/name "Erin"]])"#);
        minigraf_close(db);
        let db = minigraf_open(path.as_ptr());
        assert_eq!(db_names(db), ["Alice", "Bob", "Erin"], "after reopening");
        minigraf_close(db);
        minigraf_tx_free(std::ptr::null_mut());
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn tx_free_on_another_thread_releases_the_lock() {
        let db = tx_db();
        let tx = begin(db, None);
        tx_exec(tx, r#"(transact [[:carol :person/name "Carol"]])"#).expect("transact");
        let t = Ptr(tx);
        std::thread::spawn(move || minigraf_tx_free(t.get()))
            .join()
            .unwrap();
        let p = Ptr(db);
        let (other, _) = within(STUCK, move || {
            exec(p.get(), r#"(transact [[:x :person/name "X"]])"#);
        });
        assert!(other.is_some(), "another thread writes");
        exec(db, r#"(transact [[:y :person/name "Y"]])"#);
        assert_eq!(db_names(db), ["Alice", "Bob", "X", "Y"]);
        minigraf_close(db);
    }

    #[test]
    fn tx_failed_statement_and_finished_transaction() {
        let db = tx_db();
        let tx = begin(db, None);
        assert!(tx_exec(tx, "(transact [[:a").is_none());
        assert_eq!(tx_code(tx), "PRS-004");
        assert!(
            tx_exec(
                tx,
                r#"(transact [[:x :p/v 1 {:valid-from "2020-01-01T00:00:00Z"}] [:x :p/v 1 {:valid-from "2021-01-01T00:00:00Z"}]])"#
            )
            .is_none()
        );
        assert_eq!(tx_code(tx), "API-011");
        tx_exec(tx, r#"(transact [[:carol :person/name "Carol"]])"#).expect("still open");
        assert!(minigraf_tx_last_error(tx).is_null(), "cleared by a success");
        assert_eq!(minigraf_tx_commit(tx), 0);
        assert!(tx_exec(tx, NAMES).is_none());
        assert_eq!(tx_code(tx), "API-018");
        assert_eq!(minigraf_tx_commit(tx), -1);
        assert_eq!(tx_code(tx), "API-018");
        assert_eq!(minigraf_tx_rollback(tx), 0, "does nothing");
        assert!(minigraf_tx_last_error(tx).is_null());
        minigraf_tx_free(tx);
        assert_eq!(db_names(db), ["Alice", "Bob", "Carol"]);
        minigraf_close(db);
    }

    #[test]
    fn tx_guards_the_begin_thread_and_others_wait() {
        let db = tx_db();
        let pq = minigraf_prepare(db, c(NAMES).as_ptr());
        assert!(!pq.is_null());
        let tx = begin(db, None);
        tx_exec(tx, r#"(transact [[:carol :person/name "Carol"]])"#).expect("transact");

        let q = c(NAMES);
        assert!(minigraf_execute(db, q.as_ptr()).is_null());
        assert_eq!(db_code(db), "INT-001", "execute");
        assert!(minigraf_query(db, q.as_ptr()).is_null());
        assert_eq!(db_code(db), "INT-001", "query");
        assert!(minigraf_prepare(db, q.as_ptr()).is_null());
        assert_eq!(db_code(db), "INT-001", "prepare");
        assert!(minigraf_fact_log(db, std::ptr::null()).is_null());
        assert_eq!(db_code(db), "INT-001", "fact_log");
        assert_eq!(minigraf_checkpoint(db), -1);
        assert_eq!(db_code(db), "INT-001", "checkpoint");
        assert!(begin(db, None).is_null());
        assert_eq!(db_code(db), "INT-001", "begin");
        assert!(minigraf_prepared_execute(pq, std::ptr::null()).is_null());
        assert_eq!(code_of(&prepared_err(pq)), "INT-001", "prepared execute");

        let p = Ptr(db);
        let (early_write, write) = within(WAITS, move || {
            exec(p.get(), r#"(transact [[:w :person/name "W"]])"#);
        });
        assert!(early_write.is_none(), "a write from another thread waits");
        let (early_begin, begun) = within(WAITS, move || {
            let tx = begin(p.get(), None);
            assert!(!tx.is_null());
            tx_exec(tx, r#"(transact [[:b :person/name "B"]])"#).expect("transact");
            assert_eq!(minigraf_tx_commit(tx), 0);
            minigraf_tx_free(tx);
        });
        assert!(early_begin.is_none(), "a begin from another thread waits");
        let (query, _) = within(STUCK, move || db_names(p.get()));
        assert_eq!(
            query.expect("a query runs while a begin waits"),
            ["Alice", "Bob"]
        );

        assert_eq!(minigraf_tx_commit(tx), 0);
        minigraf_tx_free(tx);
        write.recv_timeout(STUCK).expect("the write ends");
        begun.recv_timeout(STUCK).expect("the begin ends");
        assert_eq!(db_names(db), ["Alice", "B", "Bob", "Carol", "W"]);
        minigraf_prepared_free(pq);
        minigraf_close(db);
    }

    #[test]
    fn tx_used_from_several_threads() {
        let db = tx_db();
        let tx = Ptr(begin(db, None));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                std::thread::spawn(move || {
                    for j in 0..20 {
                        tx_exec(tx.get(), &format!("(transact [[:t{i}_{j} :p/n {j}]])"))
                            .expect("a whole statement");
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        // Committed on another thread than the one that began it.
        let committed = std::thread::spawn(move || minigraf_tx_commit(tx.get()))
            .join()
            .unwrap();
        assert_eq!(committed, 0);
        let count = take(minigraf_execute(
            db,
            c("(query [:find (count ?e) :where [?e :p/n _]])").as_ptr(),
        ))
        .unwrap();
        assert!(count.contains("[[160]]"), "every statement committed");
        minigraf_tx_free(tx.get());
        exec(db, r#"(transact [[:z :person/name "Z"]])"#);
        minigraf_close(db);
    }

    #[test]
    fn tx_options_and_read_only() {
        let (file, path) = tmp("tx_options");
        let db = minigraf_open(path.as_ptr());
        exec(db, TX_SETUP);
        assert!(begin(db, Some(r#"{"chunk_facts": -1}"#)).is_null());
        assert_eq!(db_code(db), "API-017", "negative chunk_facts");
        assert!(begin(db, Some(r#"{"chunk": 3}"#)).is_null());
        assert_eq!(db_code(db), "API-017", "unknown key");
        assert!(begin(db, Some(r#"{"synchronous": "sometimes"}"#)).is_null());
        assert_eq!(db_code(db), "API-017", "bad synchronous");
        let tx = begin(db, Some(r#"{"synchronous": "normal", "chunk_facts": 3}"#));
        assert!(!tx.is_null());
        for i in 0..4 {
            tx_exec(
                tx,
                &format!(
                    "(transact [[:c{i}a :person/name \"C{i}a\"] [:c{i}b :person/name \"C{i}b\"]])"
                ),
            )
            .expect("transact");
        }
        assert_eq!(names_of(&tx_exec(tx, NAMES).unwrap()).len(), 10);
        assert_eq!(minigraf_tx_commit(tx), 0);
        minigraf_tx_free(tx);
        minigraf_close(db);
        let db = minigraf_open(path.as_ptr());
        assert_eq!(db_names(db).len(), 10, "every fact after reopening");
        minigraf_close(db);
        let ro = minigraf_open_with_options(
            path.as_ptr(),
            c(r#"{"read_only": true}"#).as_ptr(),
            std::ptr::null_mut(),
        );
        assert!(begin(ro, None).is_null());
        assert_eq!(db_code(ro), "API-014");
        minigraf_close(ro);
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn close_rolls_back_an_open_transaction() {
        let (file, path) = tmp("tx_close");
        let db = minigraf_open(path.as_ptr());
        exec(db, TX_SETUP);
        let tx = begin(db, None);
        tx_exec(tx, r#"(transact [[:carol :person/name "Carol"]])"#).expect("transact");
        minigraf_close(db);
        assert!(tx_exec(tx, NAMES).is_none());
        assert_eq!(tx_code(tx), "API-018", "execute after close");
        assert_eq!(minigraf_tx_commit(tx), -1);
        assert_eq!(tx_code(tx), "API-018", "commit after close");
        assert_eq!(minigraf_tx_rollback(tx), 0);
        // The file lock was released: the path reopens.
        let db = minigraf_open(path.as_ptr());
        assert!(!db.is_null());
        assert_eq!(db_names(db), ["Alice", "Bob"]);
        minigraf_close(db);
        minigraf_tx_free(tx); // after the close, still safe
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    unsafe extern "C" fn carol_only(
        _: *mut MiniGrafUdfContext,
        _: *mut c_void,
        value_json: *const c_char,
    ) -> c_int {
        let v = unsafe { CStr::from_ptr(value_json) }.to_str().unwrap();
        c_int::from(v.contains("Carol"))
    }

    unsafe extern "C" fn calls_back(
        ctx: *mut MiniGrafUdfContext,
        user_data: *mut c_void,
        _: *const c_char,
    ) -> c_int {
        let db = user_data.cast::<MiniGrafDb>();
        let q = CString::new(NAMES).unwrap();
        let r = minigraf_execute(db, q.as_ptr());
        if r.is_null() {
            let msg = unsafe { CStr::from_ptr(minigraf_last_error(db)) }.to_owned();
            minigraf_udf_error(ctx, msg.as_ptr());
            return -1;
        }
        minigraf_string_free(r);
        1
    }

    #[test]
    fn tx_user_functions() {
        let db = tx_db();
        assert_eq!(
            minigraf_register_predicate(
                db,
                c("carol?").as_ptr(),
                Some(carol_only),
                std::ptr::null_mut(),
                None
            ),
            0
        );
        assert_eq!(
            minigraf_register_predicate(
                db,
                c("calls-back?").as_ptr(),
                Some(calls_back),
                db.cast(),
                None
            ),
            0
        );
        let tx = begin(db, None);
        tx_exec(tx, r#"(transact [[:carol :person/name "Carol"]])"#).expect("transact");
        let seen = tx_exec(
            tx,
            "(query [:find ?n :where [?e :person/name ?n] [(carol? ?n)]])",
        )
        .expect("a user function sees staged values");
        assert_eq!(names_of(&seen), ["Carol"]);
        assert!(
            tx_exec(
                tx,
                "(query [:find ?n :where [?e :person/name ?n] [(calls-back? ?n)]])"
            )
            .is_none()
        );
        assert_eq!(tx_code(tx), "QRY-014");
        assert!(text(minigraf_tx_last_error(tx)).contains("INT-001"));
        assert_eq!(minigraf_tx_commit(tx), 0);
        minigraf_tx_free(tx);
        assert_eq!(db_names(db), ["Alice", "Bob", "Carol"]);
        minigraf_close(db);
    }

    /// Calls the transaction whose statement runs it (minigraf#886): 1 if
    /// every call was refused with INT-001, else 0.
    unsafe extern "C" fn calls_its_tx(
        _: *mut MiniGrafUdfContext,
        user_data: *mut c_void,
        _: *const c_char,
    ) -> c_int {
        let tx = user_data.cast::<MiniGrafTx>();
        let q = CString::new(NAMES).unwrap();
        let executed = minigraf_tx_execute(tx, q.as_ptr());
        let mut refused = executed.is_null() && tx_code(tx) == "INT-001";
        minigraf_string_free(executed);
        refused &= minigraf_tx_commit(tx) == -1 && tx_code(tx) == "INT-001";
        refused &= minigraf_tx_rollback(tx) == -1 && tx_code(tx) == "INT-001";
        c_int::from(refused)
    }

    /// As `calls_its_tx`, passing the refusal on as its own failure.
    unsafe extern "C" fn commits_its_tx(
        ctx: *mut MiniGrafUdfContext,
        user_data: *mut c_void,
        _: *const c_char,
    ) -> c_int {
        let tx = user_data.cast::<MiniGrafTx>();
        if minigraf_tx_commit(tx) == 0 {
            return 1;
        }
        let msg = unsafe { CStr::from_ptr(minigraf_tx_last_error(tx)) }.to_owned();
        minigraf_udf_error(ctx, msg.as_ptr());
        -1
    }

    #[test]
    fn tx_refuses_a_call_from_its_own_statement() {
        let db = tx_db();
        let tx = begin(db, None);
        tx_exec(tx, r#"(transact [[:carol :person/name "Carol"]])"#).expect("transact");
        for (name, f) in [
            (
                "calls-tx?",
                calls_its_tx as unsafe extern "C" fn(_, _, _) -> _,
            ),
            ("commits-tx?", commits_its_tx),
        ] {
            assert_eq!(
                minigraf_register_predicate(db, c(name).as_ptr(), Some(f), tx.cast(), None),
                0
            );
        }
        let (tx, db) = (Ptr(tx), Ptr(db));

        let (rows, _recv) = within(STUCK, move || {
            tx_exec(
                tx.get(),
                "(query [:find ?n :where [?e :person/name ?n] [(calls-tx? ?n)]])",
            )
        });
        let rows = rows
            .expect("the statement returns: no deadlock")
            .expect("every call was refused, so every row passes");
        assert_eq!(names_of(&rows), ["Alice", "Bob", "Carol"]);
        assert!(
            minigraf_tx_last_error(tx.get()).is_null(),
            "the statement succeeded"
        );

        let (failed, _recv) = within(STUCK, move || {
            tx_exec(
                tx.get(),
                "(query [:find ?n :where [?e :person/name ?n] [(commits-tx? ?n)]])",
            )
        });
        assert!(
            failed
                .expect("the statement returns: no deadlock")
                .is_none(),
            "the function passed the refusal on"
        );
        assert_eq!(tx_code(tx.get()), "QRY-014");
        assert!(text(minigraf_tx_last_error(tx.get())).contains("INT-001"));

        // Neither the refused commits nor the refused rollback finished it.
        assert_eq!(minigraf_tx_commit(tx.get()), 0);
        minigraf_tx_free(tx.get());
        assert_eq!(db_names(db.get()), ["Alice", "Bob", "Carol"]);
        minigraf_close(db.get());
    }

    #[test]
    fn tx_rules_stay_after_rollback() {
        let db = tx_db();
        let tx = begin(db, None);
        tx_exec(tx, "(rule [(named ?e ?n) [?e :person/name ?n]])").expect("rule");
        minigraf_tx_rollback(tx);
        minigraf_tx_free(tx);
        let r = take(minigraf_execute(
            db,
            c("(query [:find ?n :where (named ?e ?n)])").as_ptr(),
        ))
        .expect("the rule is still registered");
        assert_eq!(names_of(&r), ["Alice", "Bob"]);
        minigraf_close(db);
    }
}
