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

Each archive contains `libminigraf.{so|dylib|dll}` and `minigraf.h`.

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

Compile: `cc -o example example.c -Iinclude -L. -lminigraf -Wl,-rpath,.`

## Memory contract

- Strings returned by `minigraf_execute`, `minigraf_cursor_vars`,
  `minigraf_cursor_next_batch` and `minigraf_fact_log_next_batch`, and messages stored
  in an `error_out`, must be freed with `minigraf_string_free`.
- Databases must be closed with `minigraf_close`; cursors, fact logs and log writers
  must be freed with `minigraf_cursor_free`, `minigraf_fact_log_free` and
  `minigraf_log_writer_free`.
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
| `minigraf_open_with_options(path, options_json, &err)` | Open with options (`read_only`, `page_cache_size`, `allow_unlocked`, `wal_checkpoint_threshold`, `max_derived_facts`, `max_results`, `synchronous`: `"full"`/`"normal"`); NULL options = defaults; on error returns NULL and stores the message in `err` |
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

A fact record is `{"entity": "<uuid>", "attribute": ":a/b", "value": {"type": "string", "value": "x"},
"tx_count": 1, "tx_id": 1700000000000, "valid_from": 1700000000000, "valid_to": 9223372036854775807,
"asserted": true}`. Value types are `string`, `integer`, `float`, `boolean`, `ref` (a UUID string),
`keyword` and `null` (no `value`), so a record is written back exactly. A `valid_to` of
9223372036854775807 means forever. The batches `minigraf_fact_log_next_batch` returns can be passed
straight to `minigraf_log_writer_append_batch`; see `tests/c/etl_smoke.c`.

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
