# Factory packaging

The plugin build stays a non-`state-store` binary. `herdr-plugin.toml` runs
`["cargo", "build", "--release", "--locked"]` and does not pass
`--features state-store`. That line changes only in a later release record.

The explicit factory binary is a second build:

```sh
cargo build --release --locked --features state-store
```

`herdr-projects doctor` prints whether `state-store` is compiled, the schema
that binary can write, the linked SQLite version, and `prepared_dispatch`.
Those values are the compiled constants, not a project the command opened:

- `pub const SCHEMA: u32 = 26` in `src/store/mod.rs`
- `rusqlite::version()` of the SQLite library linked into that binary
- `const PREPARED_LAUNCH_DISPATCH_ENABLED: bool = true` in
  `src/canonical_controller.rs`, printed as `prepared_dispatch`

A build without `state-store` has no store module and no canonical controller,
so doctor prints `schema: absent`, `sqlite: absent`, `prepared_dispatch: absent`,
and says canonical factory commands are absent. It still names the factory
build above.

Doctor does not migrate. It does not call `upgrade_v1` or
`migration PROJECT upgrade-store`. Existing projects upgrade only through
`migration PROJECT upgrade-store` on a `state-store` binary. `SqliteStore::open`
does not migrate. See [factory baseline](baseline.md) and
[migration workflow](../migration-workflow.md).
