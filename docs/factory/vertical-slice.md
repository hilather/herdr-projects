# Deterministic dependent vertical slice

`tests/factory_harness.rs` builds a disposable repository and reserves dependents from stored evidence. Task A changes a function. B reserves only after A's commit is integrated onto the local ref. C reserves from A's verified result and does not need that commit in its tree. D reserves only when the pinned base contains both integrated parents. A second change passes on its own branch and fails the combined-tree check, so it does not publish and does not satisfy an edge. A crash after `update-ref` and before the integration receipt is recovered by reading the ref back.

The harness writes a manifest with `vertical_slice: pass` and the git SHA of this tree. It turns `factory_admission` on by running `UPDATE` against the fixture database. The migration default stays `off`. There is no release-build setter. `PREPARED_LAUNCH_DISPATCH_ENABLED` is unchanged.

The live Codex repository-editing cell is not run. This slice does not start an agent, open a network, or mark a capability row repository-capable.
