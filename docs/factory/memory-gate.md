# Memory gate

Deterministic simulator for memory concurrency. This is not a live pilot. No production flag was flipped.

The harness in `tests/factory_harness.rs` opens disposable schema-40 stores and drives the store APIs. It does not call a live provider. It does not poll pull requests. It does not use network git.

Six scenarios:

1. Two domains promote together. Two independent memory promotions both commit when they do not share a head.
2. Same-head conflict. Two promotions that expect the same event head: the second conflicts and does not write a second promotion.
3. Subscription retirement. Retiring a consumer binding keeps the unresolved obligation on the successor. A retired snapshot does not receive a new obligation. The pending obligation is not deleted.
4. Barrier invalidation. A membership edit invalidates the old release token. Revocation does not set `termination_observed`.
5. Package gap. An ack of an older package does not apply a change id that is not on that package.
6. Coordinator receipt without an attempt. A coordinator binding with `attempt_id` NULL can ack a package and does not insert an attempts row.

`factory_admission` stays `off` in production code. The harness does not update that column. `PREPARED_LAUNCH_DISPATCH_ENABLED` is unchanged. This simulator does not claim a live 40-worker certificate or a latency bar.
