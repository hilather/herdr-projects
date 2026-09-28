# F1 fixture gate (card 4)

Date: 27 September 2026. Test:
`ticker_auto_chain_releases_verified_integrated_and_fan_in_dependents`
(`tests/cli.rs`).

## What it proves

With `hp result <slug> auto --verify on --integrate on`, and only the ticker
working after the submissions:

- Two submitted results (`a`, `e`) whose workers claim success release nothing
  until verification evidence exists: every edge reports missing evidence and
  `dependency_satisfactions` is empty.
- `b` (verified result of `a`) is released after automatic verification, while
  `a`'s integration is still in progress.
- `c` (integrated commit of `a`) stays blocked after verification alone and is
  released after automatic integration; its satisfaction names `a`'s integrated
  commit, whose second parent is `a`'s candidate.
- Fan-in `d` (integrated commits of `a` and `e`) is released only when both
  are integrated; the target tip contains both integrated commits and both
  candidates.
- A `kill -9` of the ticker between `a`'s verification and its integration
  yields one run per result, two integrations, two publications and exactly one
  satisfaction per edge.
- A fresh clone of the target ref holds both results and passes the policy.

No production glue was needed.

## Limits

This is fixture-only evidence: local SHA-256 git repositories, a local
git-protocol fixture holding one reply, and hand-built submissions. No worker
was launched and no live or paid service was used. Consumers carry queue edges
only, and factory admission stays off, so the test shows readiness of the
dependency edges, not a launch.

## What the live F1.7 check must still prove

A real authenticated worker produces the result; it is verified and integrated
automatically; a dependent task actually launches on the integrated SHA; one
controller crash and one stale-head injection are survived. Record the
integrated SHA, transcripts, and which parts were live versus fixture.
