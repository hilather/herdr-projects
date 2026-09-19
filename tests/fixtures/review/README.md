# T00.1 fixture provenance

Baseline: `6e2bd7607d7bc64cf6155beceb299b44566d7f3b`, captured 2026-09-19.

- `cli-help.txt`: actual output of the baseline `herdr-projects --help`, built with
  Rust 1.89 and the checked-in lockfile. Documentary capture, not a whitespace
  snapshot test; update intentionally when the public CLI changes.
- `merged-pr.json`: synthetic response matching the existing `gh pr view`
  adapter contract, reused unchanged across failure, restart and recovery.
  The fixture's repository and branch match the temporary scenario's thread.
- Herdr agent/pane/machine JSON in `src/scenarios/review_regressions.rs` is
  synthetic and derives from the existing `World` fixtures. Each project has a
  distinct session socket but shares machine `box` and thread ID `t-0001`.

No JSON here was obtained from an authenticated account or live Herdr session.
No fixture identifies a certified agent/version. See
[the baseline report](../../../docs/review-baseline.md) for commands and limits.
