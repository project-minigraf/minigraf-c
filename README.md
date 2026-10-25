# minigraf-c

C bindings for [Minigraf](https://github.com/project-minigraf/minigraf) — zero-config,
single-file, embedded bi-temporal graph database.

## Installation

Download the pre-built library for your platform from the
[latest release](https://github.com/project-minigraf/minigraf-c/releases/latest):

| Platform | Archive |
|---|---|
| Linux x86_64 | `minigraf-c-<version>-linux-x86_64.tar.gz` |
| Linux aarch64 | `minigraf-c-<version>-linux-aarch64.tar.gz` |
| macOS universal | `minigraf-c-<version>-macos-universal2.tar.gz` |
| Windows x86_64 | `minigraf-c-<version>-windows-x86_64.zip` |

Each archive contains the shared library (`libminigraf.so`, `libminigraf.dylib` or
`minigraf.dll`), `include/minigraf.h` and the license files. The Windows archive also has
`minigraf.dll.lib`, the import library MSVC links against. The Linux libraries need glibc
2.17 or later.

## Quick start

```c
#include "minigraf.h"
#include <stdio.h>

int main(void) {
    MiniGrafDb *db = minigraf_open_in_memory();
    char *result = minigraf_execute(db, "(transact [[:alice :name \"Alice\"]])");
    printf("%s\n", result);
    minigraf_string_free(result);
    minigraf_close(db);
    return 0;
}
```

Compile, from the unpacked archive:

- Linux and macOS: `cc -o example example.c -Iinclude -L. -lminigraf -Wl,-rpath,.`
- Windows (MSVC): `cl /Iinclude example.c minigraf.dll.lib`, with `minigraf.dll` next to `example.exe`

## Memory contract

- Strings returned by `minigraf_execute`, `minigraf_cursor_vars`,
  `minigraf_cursor_next_batch` and `minigraf_fact_log_next_batch`, and messages stored
  in an `error_out`, must be freed with `minigraf_string_free`.
- Databases must be closed with `minigraf_close`; cursors, fact logs, log writers and
  prepared queries must be freed with `minigraf_cursor_free`, `minigraf_fact_log_free`,
  `minigraf_log_writer_free` and `minigraf_prepared_free`. `minigraf_close` releases
  the database's prepared queries (later calls fail with `API-018`), but each must
  still be freed.
- Strings returned by the `*_last_error` functions belong to the object and are valid
  until its next call. Every error message starts with its code, such as `[API-015]`.
- Passing `NULL` to any function is safe (no-op or returns NULL/error).

## API

| Function | Description |
|---|---|
| `minigraf_open(path)` | Open a file-backed database |
| `minigraf_open_in_memory()` | Open an in-memory database |
| `minigraf_execute(db, datalog)` | Execute Datalog, returns JSON string |
| `minigraf_string_free(s)` | Free a string returned by `execute` |
| `minigraf_checkpoint(db)` | Flush WAL to disk; returns 0 on success |
| `minigraf_last_error(db)` | Return last error message (valid until next call) |
| `minigraf_close(db)` | Close the database and free all memory |
| `minigraf_open_with_options(path, options_json, &err)` | Open with options (`read_only`, `page_cache_size`, `allow_unlocked`, `wal_checkpoint_threshold`, `max_derived_facts`, `max_results`, `synchronous`: `"full"`/`"normal"`, `query_memory_limit`: bytes one query may hold, else `QRY-012`, default no limit); NULL options = defaults; on error returns NULL and stores the message in `err` |
| `minigraf_current_tx_count(db, &out)` | The transaction counter `:as-of N` compares against |
| `minigraf_query(db, datalog)` | Open a cursor; its answer is fixed when it opens |
| `minigraf_cursor_vars(c)` | The `:find` variables, as a JSON array |
| `minigraf_cursor_next_batch(c, n)` | Up to `n` rows as JSON (like `execute`'s `results`); NULL at the end or on error (`minigraf_cursor_last_error`) |
| `minigraf_cursor_free(c)` | Close and free a cursor |
| `minigraf_fact_log(db, filter_json)` | Stream every fact version; filter keys `attributes`, `attribute_prefixes`, `entities`, `tx_from`, `tx_to`, `order` (`"tx"`/`"storage"`), `window`; NULL = all |
| `minigraf_fact_log_next_batch(log, n)` | Up to `n` records as a JSON array (see below); NULL at the end or on error (`minigraf_fact_log_last_error`) |
| `minigraf_fact_log_free(log)` | Close a fact log, releasing the database |
| `minigraf_log_writer_create(path, options_json, &err)` | Start building a new file from records (`STG-043` if it exists) |
| `minigraf_log_writer_append(w, record_json)` / `_append_batch(w, records_json)` | Append records in transaction order; a rejected batch record's error ends with `(batch index N)` |
| `minigraf_log_writer_advance_tx_count(w, n)` / `_tx_count(w, &out)` | Raise / read the writer's transaction counter |
| `minigraf_log_writer_finish(w)` | Commit and rename the file into place; later calls fail with `API-018` |
| `minigraf_log_writer_last_error(w)` | The error of the writer's last call |
| `minigraf_log_writer_free(w)` | Free the writer; an unfinished build is abandoned and leaves no file |
| `minigraf_prepare(db, datalog)` | Parse a query with `$slots` once; NULL on error (`minigraf_last_error`: `API-005`–`007` for a non-query, `API-023` for an attribute slot) |
| `minigraf_prepared_execute(pq, bindings_json)` | Run it with `bindings_json` (see below; NULL = `{}`) and return JSON like `execute`; NULL on error (`minigraf_prepared_last_error`) |
| `minigraf_prepared_query(pq, bindings_json)` | Run it and return a cursor |
| `minigraf_prepared_last_error(pq)` | The error of its last call: `API-017` (bad bindings), `API-022` (missing slot), `API-024` (wrong kind), `API-018` (database closed) |
| `minigraf_prepared_free(pq)` | Free a prepared query, also after `minigraf_close` |
| `minigraf_register_predicate(db, name, f, user_data, destroy)` | Register a predicate (see User functions); 0 on success, -1 on error (`minigraf_last_error`) |
| `minigraf_register_aggregate(db, name, init, step, finish, acc_free, user_data, destroy)` | Register an aggregate |
| `minigraf_udf_error(ctx, message)` / `minigraf_udf_result(ctx, value_json)` | From inside a callback: fail the call / set `finish`'s result |

A fact record is `{"entity": "<uuid>", "attribute": ":a/b", "value": {"type": "string", "value": "x"},
"tx_count": 1, "tx_id": 1700000000000, "valid_from": 1700000000000, "valid_to": 9223372036854775807,
"asserted": true}`. Value types are `string`, `integer`, `float`, `boolean`, `ref` (a UUID string),
`keyword` and `null` (no `value`), so a record is written back exactly. A `valid_to` of
9223372036854775807 means forever. The batches `minigraf_fact_log_next_batch` returns can be passed
straight to `minigraf_log_writer_append_batch`; see `tests/c/etl_smoke.c`.

Bindings are a JSON object of slot name (without the `$`) to bind value: `true`/`false`, a
number (an integer when it has no fraction or exponent, so `2` is an integer and `2.0` a float),
a string, `null`, or a typed object: a value type above (`{"type": "keyword", "value": ":k"}`),
`{"type": "entity", "value": "<uuid>"}`, `{"type": "txCount", "value": 5}` or
`{"type": "timestamp", "value": 1700000000000}` for `:as-of`, and `{"type": "anyValidTime"}`
for `:valid-at`. A set, for `in?` and the other set predicates or a set-valued attribute, is
`{"type": "set", "value": [1, 2, {"type": "keyword", "value": ":k"}]}`: its elements are plain
values or typed value objects. An element of another kind is `API-017` and a set inside a set
`API-021`; a plain array is not a set. Set values come back as JSON arrays from
`minigraf_execute` and as `{"type": "set", "value": [...]}` in fact records.

```c
MiniGrafPrepared *pq = minigraf_prepare(db, "(query [:find ?n :where [?e :age $age] [?e :name ?n]])");
char *thirty = minigraf_prepared_execute(pq, "{\"age\": 30}");
minigraf_string_free(thirty);
minigraf_prepared_free(pq);
```

## User functions

Specified in minigraf's
[`docs/superpowers/specs/2026-10-24-udf-uniffi-design.md`](https://github.com/project-minigraf/minigraf/blob/v3/docs/superpowers/specs/2026-10-24-udf-uniffi-design.md)
§6.2. As with SQLite's `sqlite3_create_function`, a function is a function pointer, a
`void *user_data` and an optional destructor, and a callback sets its result or error
through a `MiniGrafUdfContext`, so no string ownership crosses the boundary.

```c
/* adult?: 1 true, 0 false, -1 failed. */
static int adult(MiniGrafUdfContext *ctx, void *user_data, const char *value_json) {
    long long n;
    if (sscanf(value_json, "{\"type\":\"integer\",\"value\":%lld}", &n) != 1) {
        minigraf_udf_error(ctx, "not an integer");
        return -1;
    }
    return n >= 18;
}

/* product: one accumulator per group, freed with acc_free (here free). */
static void *product_new(MiniGrafUdfContext *ctx, void *user_data) {
    long long *acc = malloc(sizeof *acc);
    if (acc) *acc = 1;
    return acc; /* NULL fails the call */
}
static int product_step(MiniGrafUdfContext *ctx, void *acc, const char *value_json) {
    long long n;
    if (sscanf(value_json, "{\"type\":\"integer\",\"value\":%lld}", &n) != 1) return -1;
    *(long long *)acc *= n;
    return 0;
}
static int product_finish(MiniGrafUdfContext *ctx, void *acc, int64_t count) {
    char json[64];
    snprintf(json, sizeof json, "{\"type\":\"integer\",\"value\":%lld}", *(long long *)acc);
    minigraf_udf_result(ctx, json);
    return 0;
}

minigraf_register_predicate(db, "adult?", adult, NULL, NULL);
minigraf_register_aggregate(db, "product", product_new, product_step, product_finish,
                            free, NULL, NULL);
char *r = minigraf_execute(db,
    "(query [:find (product ?a) :where [?e :age ?a] [(adult? ?a)]])");
```

- A predicate is called in Datalog as `[(name ?v)]` and returns 1 (true), 0 (false) or -1
  (failed). An aggregate is used as `(name ?v)` in `:find` or with `:over`: `init` makes one
  accumulator per group or window partition, `step` adds a value (0 ok, -1 failed) and
  `finish(ctx, acc, count)` sets the aggregate with `minigraf_udf_result` (0 ok, -1 failed),
  where `count` is the number of values stepped. An aggregate steps a group's non-`nil`
  values and gets `nil` with none; a window steps every value and calls `finish` once per
  set of peer rows.
- **Values** cross as typed JSON, the fact records' `{"type": "integer", "value": 30}` form
  (sets as `{"type": "set", "value": [...]}`). `value_json` is valid during the call only.
- **Failures.** A failing call stops the query with `QRY-014`, whose message is the one given
  to `minigraf_udf_error`; it is never read as false or a missing value. The call fails when
  the callback calls `minigraf_udf_error`, whatever it then returns; when it returns -1
  (message `user function failed`); when it returns any other code it does not define; when
  `init` returns NULL; and when `finish` sets no result, JSON that is not a typed value, or a
  `ref` whose value is not a UUID. A `ctx` is valid only during the call it was passed to.
- **Lifecycle.** `destroy(user_data)`, if not NULL, runs when the last reference to the
  function drops: after `minigraf_close` and after every cursor, fact log and prepared query
  from the database is freed. Until then `user_data` must stay valid. A registration that
  fails (`API-017` for a NULL argument, `INT-040` for a name that is taken) runs `destroy`
  before it returns. `acc_free(acc)`, if not NULL, runs when the query drops the accumulator:
  after `finish`, on an error, or at an early `minigraf_cursor_free`. There is no
  unregister. Predicates and aggregates have separate names. A query sees the functions
  registered when it opened.
- **Threading.** A callback runs on the thread that calls `minigraf_execute`,
  `minigraf_cursor_next_batch` or a prepared query's execute or cursor. Queries on different
  threads may call the same function at once, so the callbacks and `user_data` must be safe to
  use from any thread that queries the database. One accumulator is used by one query on one
  thread at a time. `destroy` and `acc_free` run on the thread that drops the last reference.
- **Cost.** Each call is one C function call and one JSON encoding of the value.
- **Re-entry.** A callback may query the same database, open a cursor, write, or register
  another function: no lock of the handle is held while the core runs. A callback must not
  close the database it runs on.

## Building from source

```bash
cargo build --release
# produces target/release/libminigraf.{so|dylib|dll}
```

Regenerate the header after changing the public API:
```bash
cbindgen --config cbindgen.toml --crate minigraf-c-shim --output include/minigraf.h
```

## License

MIT OR Apache-2.0
