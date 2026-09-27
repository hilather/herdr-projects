Evidence for the review of bd6433d4d488d5d6b37701ce5707b580f076b752.

`regression-probes.patch` adds five Rust tests and a test helper only. It does not fix the implementation. Apply it in an isolated checkout of that commit and run:

```sh
cargo test --locked --features state-store --lib review_probe -- --test-threads=1 --nocapture
```

All five tests fail on the specific invariants described in the review. The wrong-policy probe runs the real isolated verifier; Linux namespace support is required for that probe.

Existing-test checks:

```sh
cargo test --locked --features state-store --lib migration::tests::published_v2_store_upgrades_explicitly_without_losing_migration_identity -- --exact --nocapture
cargo test --locked --features state-store --lib verification::tests::happy_path_checks_out_retained_objects_and_keeps_capacity -- --exact --nocapture
cargo test --locked --features state-store --test factory_harness -- --test-threads=1
cargo check --locked
```

The factory harness needs Git metadata for the reviewed commit because it records HEAD in its manifest. Tests use disposable local repositories/stores. Live-provider tests were not enabled.

`library-tests.log` is the unrestricted stable-binary rerun of the existing library suite with `--skip review_probe --test-threads=4`: 586 passed, 21 failed, 13 ignored. The equivalent Cargo selection is:

```sh
cargo test --locked --features state-store --lib -- --skip review_probe --test-threads=4
```

Avoid recompiling a test executable while its process/crash tests are running: some tests re-execute their own binary. Unix-socket and Linux namespace support are also needed for the local fixtures.
