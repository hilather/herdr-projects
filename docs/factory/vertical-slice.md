# Deterministic dependent vertical slice

`tests/factory_harness.rs` builds a disposable repository. Task A changes a function. B's integrated-commit edge and C's verified-result edge are stored from that evidence. D's two integrated parents are stored the same way. A second change passes on its own branch and fails the combined-tree check, so it does not publish and does not satisfy an edge. A crash after `update-ref` and before the integration receipt is recovered by reading the ref back. The harness calls `admit_once` only when the flag is off and when a two-parent pin is not an ancestor (`integration_missing`).

The success wake is not a harness command. `poll_reserves_one_dependent_only_when_factory_admission_is_already_on` calls `poll_queued_effects` in-process. SQL turns `factory_admission` on, C is the only queued dependent, and that one wake moves C from 0 attempts to 1 without giving any other task an attempt. The migration default stays `off`. There is no release-build setter and no debug poll command. `PREPARED_LAUNCH_DISPATCH_ENABLED` is unchanged.

The harness writes a manifest with `vertical_slice: pass` and the git SHA of this tree.

The live Codex repository-editing cell is not run. This slice does not start an agent, open a network, or mark a capability row repository-capable.
