# Direct-launch readiness compatibility

Verified against installed Herdr 0.9.1, protocol 22, and the local source checkout
at `/tmp/herdr-canonical-source`. Installed Codex: 0.154.0.

The disposable `live_vendor_direct_exec_and_native_naming_contract` test launches
an exact native Codex executable through the production isolated gate command.
It observes the supervisor and direct process, releases the gate once, observes
native agent recognition, renames the agent, checks the terminal identity, and
verifies namespace termination after workspace closure. Temporary HOME, no
credentials and no task prompt are used. Native `interactive_ready` is absent or
false and the sampled unauthenticated screen has no positive visible idle match.

The live evidence exposed an incompatible readiness assumption. Herdr's
`TerminalState::managed_agent_interactive_ready` reports the managed launch phase;
a layout command followed by external exec and `agent.rename` never enters that
phase. Requiring that field made initial brief delivery impossible for the
canonical direct-executable path.

The worker adapter now requires native idle identity plus `agent.explain` evidence:
positive visible idle, a matched idle rule from the bundled manifest, no blocker,
working state, skipped detection, fallback, override or warning. Merely detecting
a known process defaults to idle in Herdr and is insufficient. The adapter rereads
agent identity after inspecting detection, and repeats readiness validation just
before submitting a brief, followed by current claim validation. Detector screen
previews are neither retained nor printed by the adapter.

Fixture regressions cover positive direct-launch readiness without the managed
flag, negative/unknown/malformed detection, and readiness lost after the one-use
claim. This is not authenticated prompt or full workflow certification. Those
capabilities remain unresolved until live authenticated acceptance is completed.

Reproduce the disposable native contract with the state-store live test target,
`HP_LIVE_HERDR` set to the exact Herdr binary, `HP_LIVE_CODEX` set to the exact native
Codex binary, and the test name above with `--ignored --nocapture`.
CLI options were checked against the installed binary's help and the
[official CLI reference](https://learn.chatgpt.com/docs/developer-commands?surface=cli).
