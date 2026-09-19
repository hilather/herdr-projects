# SQLite compatibility probe

Disposable native T00.2 probe, separate from the application dependency graph.
Requires Rust 1.89+, pkg-config and system SQLite 3.53.4+ with development headers.
Run from the repository root:

```sh
CARGO_TARGET_DIR=/tmp/herdr-sqlite-smoke-target cargo run --locked --manifest-path contracts/sqlite-smoke/Cargo.toml
```

Uses only an in-memory database. This is not the W03 store or a migration tool.
See [ADR 0002](../../docs/adr/0002-phase-b-contracts.md) for selection and limits.
