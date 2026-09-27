# Factory packaging

The factory runtime is an explicit Linux build with `state-store`. The plugin's
ordinary build remains `cargo build --release --locked`; its commands still use
`target/release/herdr-projects`. Building a factory package does not replace that
binary, change the plugin manifest, start a ticker, migrate projects, or enable
admission. The factory's operational acceptance gates remain incomplete.

From the source checkout, with a Linux Rust toolchain and SQLite development
files available, run:

```sh
scripts/build-factory
```

The script builds for the Rust host triple under `target/factory`, validates the
linked runtime, and prints a new package directory under `target/factory/packages`.
Each package contains `herdr-projects`, `build-info.json`, this guide, and
`SHA256SUMS`. Existing package directories and the plugin executable are retained.
This is a host-native dynamically linked distribution, not a portable static
binary. SQLite **3.53.4 or later** must be available on the machine where it runs.
Use the same validation after copying a package to another compatible Linux host.

For a manual host-default build, keep the separate target directory:

```sh
cargo build --release --locked --features state-store --target-dir target/factory
```

Inspect the exact binary you intend to run (replace the example package path):

```sh
factory_bin=/absolute/path/to/package/herdr-projects
"$factory_bin" build-info
"$factory_bin" build-info --require-factory
```

`build-info` emits JSON without reading config/projects or contacting sessions.
It reports compiled features, schema, OS/architecture, linked SQLite and minimum
version, prepared dispatch, and runtime compatibility. `--require-factory` exits
unsuccessfully for missing `state-store`, unsupported OS, or old SQLite, with
setup guidance. Missing shared libraries are reported by the host loader before
the command can run. Runtime compatibility does not certify adapters or fleet
capacity; `live_capacity_certified` remains false.

The disposable acceptance probe requires Python 3 and a C compiler. Capture the
prior binary's SHA-256 before packaging, then run:

```sh
scripts/test-factory-package /path/to/package /path/to/prior/herdr-projects PRIOR_SHA256
```

It verifies package checksums, prior-binary preservation, legacy readability, and
no migration during inspection. Its old-SQLite refusal check interposes version
functions in a temporary process; its unsupported-platform preflight mocks
`uname`. These are refusal probes, not old-library or macOS certifications.

`doctor` additionally inspects the user's setup. Neither command migrates. The
schema reported by the exact binary is authoritative (currently 43); a default
build reports no canonical schema or SQLite. macOS and live SSH canonical worker
execution remain unsupported. A Linux build alone does not grant launch authority.

## Opt-in onboarding

Keep the prior package and its checksum before changing a deployment. Use the
factory binary by its explicit path throughout onboarding. The existing plugin
continues to use its own executable until an operator deliberately changes the
deployment; merely compiling this package does not select it for plugin actions.

For a legacy project, first use `migration PROJECT inspect`, `preflight`, and
`plan --output /outside/project/plan.json`. Review blockers and the exact source
fingerprints. Pause/archive the project and stop its writers before applying:

```sh
"$factory_bin" --root /projects migration PROJECT apply --plan /outside/project/plan.json --writers-stopped
"$factory_bin" --root /projects migration PROJECT status
```

`--writers-stopped` records an operator prerequisite; it does not stop processes.
The migration journal, backups and ownership checks remain mandatory. For an
already canonical older store, use `migration PROJECT upgrade-store` explicitly;
opening it does not upgrade it. Reconcile after cutover/upgrade before authorizing
new effects. Admission still requires its signed policy and evidence.

| Existing state | Intended path | Failure/recovery boundary |
| --- | --- | --- |
| Legacy, no migration prepared | Prior plugin or explicit factory binary's legacy commands | Packaging alone leaves project bytes and ownership unchanged. |
| Migration prepared, not published | Same factory binary: `migration PROJECT status/recover/abort` | Legacy writers refuse the guarded root; abort preserves staged bytes. |
| Published canonical schema supported by the binary | Explicit factory binary; `upgrade-store` only when selected | No automatic migration, downgrade, or fallback to legacy execution. |
| Canonical schema newer than the binary | Compatible newer factory binary | Old writers refuse; keeping the old executable is not database rollback. |
| Corrupt/interrupted canonical state | Inspect status and recover forward; reviewed restore into a new root | Do not delete ownership markers, overwrite the database, replay grants, or reset shared refs. |

Restoring pre-cutover bytes into a new root is distinct from canonical backup
recovery. Full canonical recovery/live rollout certification remains outstanding.
See the source checkout's [migration workflow](../migration-workflow.md),
[factory operations](operations.md), and [fault/recovery notes](faults.md).
