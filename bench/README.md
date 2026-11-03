# Nightly binding benchmarks (Refs project-minigraf/minigraf#776 P16)

Run from this worktree after building the local binding. No released package
is loaded as a fallback. Build on Linux:

```sh
CARGO_TARGET_DIR="$PWD/target/p16" cargo build --release --lib; cc -O2 -std=c11 -Wall -Wextra -Werror -Iinclude bench/nightly.c target/p16/release/libminigraf.a -lpthread -ldl -lm -o target/p16/nightly
target/p16/nightly --test
```

`--test` invokes each operation exactly once, checks fixed answers and reads
no timer. Omit it only in GitHub Actions to collect median latency (nanoseconds)
in `binding-results.json`. Do not run timed cases on a developer laptop or
on the occupied benchmark machine.

IDs use `binding/c/` and the six suffixes
`prepared_execute/1k`, `cursor_batch_1/1k`, `cursor_batch_256/1k`,
`set_roundtrip/2`, `predicate/1k`, `aggregate/1k`.

Fixtures contain 1,000 deterministic integer facts and one two-element set.
Setup, prepare, registration and correctness checks are outside measurement;
foreign result decoding and cursor close are included. Set binding containers
are created per call. Callbacks are implemented in this host language.

Routing, build provenance, dedicated Bencher testbeds, provisional 5–15 minute
allowance plus setup per job, and manual coverage dispositions are documented
in [the core binding benchmark guide](https://github.com/project-minigraf/minigraf/blob/v3/docs/BINDING_BENCHMARKS.md).
The core `bindings` matrix builds this shim against the measured core SHA
using a temporary Cargo patch; this repository's production dependency pin is
unchanged. Land this PR before the core workflow PRs. Phase B performs the
serial workflow dispatch and upload/history validation.
