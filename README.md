> **This repository has moved to [hilather/herdr-farm](https://github.com/hilather/herdr-farm).**
> The project is now called herdr-farm. This repository is archived and read-only; see the new repository for current code, docs and releases, and its `docs/renaming.md` for upgrading an existing install.

# herdr-farm

herdr-farm coordinates coding agents in [Herdr](https://herdr.dev). Keep one coordinator conversation, give workers isolated tasks and worktrees, and inspect results, approvals and usage from a local project store. This repository is maintained as [hilather/herdr-farm](https://github.com/hilather/herdr-farm).

## Features

- Canonical workers through `herdr-farm launch PROJECT run`: Codex and Claude, shared owner login or setup-token login, frozen launch specifications and Linux worker sandboxing.
- Result submission, verification and integration, with automatic completion after accepted results reach the integration ref.
- Telemetry collectors, a usage ledger and published-rate cost accounting; outcomes, views, health, export and comparisons; assignment policies in shadow mode and a workspace pane.
- Live-certified agent versions, with certification evidence and explicit admission requirements.
- Coordinator conversations, shared project memory, parallel threads and an overview of work that needs operator attention.
- An [operator runbook](docs/operator-runbook.md) for launch, recovery, review and ongoing operations.

## Quick start

Install Rust and Herdr 0.9.1 or later, then build the CLI and install the plugin:

```sh
cargo build --release --locked --features state-store
herdr plugin install /path/to/herdr-farm
/path/to/herdr-farm/target/release/herdr-farm new demo
/path/to/herdr-farm/target/release/herdr-farm doctor
/path/to/herdr-farm/target/release/herdr-farm open demo
```

Add `target/release` to your `PATH` to use `herdr-farm` directly. Start with [getting started](docs/getting-started.md), then [profiles](docs/profiles.md) and [canonical worker launch](docs/canonical-worker-launch.md). Canonical launches require the signed policies and approvals described there; creating a project alone does not authorize workers.

New installations use `~/.herdr-farm` and `~/.config/herdr-farm/config.toml`. Existing installations retain their old locations through automatic selection, without copying or moving data. `HERDR_FARM_*` takes precedence over legacy `HERDR_PROJECTS_*`. See [rename compatibility and manual migration](docs/renaming.md).

## Credits

herdr-farm started as a fork of [herdr-projects](https://github.com/eliasstravik/herdr-projects) by Elias Stravik (MIT).

The MIT license and original copyright are retained in [LICENSE](LICENSE).
