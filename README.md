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

Compile: `cc -o example example.c -L. -lminigraf -Wl,-rpath,.`

## Memory contract

- Strings returned by `minigraf_execute` must be freed with `minigraf_string_free`.
- Databases must be closed with `minigraf_close`.
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
