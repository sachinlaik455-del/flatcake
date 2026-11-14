# Contributing to FlatCake

## Build and test

```bash
cargo build --release
cargo test --release          # all suites
cargo clippy --release -- -D warnings
./target/release/fckv.exe selftest
./target/release/fckv.exe demo
```

The project uses **zero external crates**. Do not add dependencies without a strong justification.

## Code style

- Rust edition 2021, std-only. No `unsafe` blocks in new code.
- Comments only where the reasoning is non-obvious. No decorative separators or restating code.
- Every new test must verify a real property (byte equality, error kind, counter bound) — no smoke asserts.

## Architecture invariants

These must not be weakened:

- `registry → {index, arena, layout}` lock ordering (see `src/gva/mod.rs`).
- Publish-after-write: the key→GVA entry is visible only after the payload write completes.
- Immutable blocks: published payloads never mutate.
- Migration frees are deferred until no in-flight reader/writer can touch the extent.
- `logical_write_bytes` is counted only on successful placement.

## Testing

- Deterministic tests (fixed seeds, no sleeps for ordering).
- Per-suite targets: `cargo test --release --test <suite_name>`.
- Concurrency tests must pass 3× consecutively before submitting.
- New suites go in `tests/` with a `validation_` or `<phase>_` prefix.

## Documentation

Update `docs/` when changing architecture, adding/removing modules, or altering public behavior. Keep `README.md` accurate.
