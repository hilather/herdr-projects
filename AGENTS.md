# Agent instructions

<!-- agent-skills-pause:start -->
## Agent-skills pause

Do not load, follow, or consult `~/git/agent-skills` (`HINTS.md`, `knowledge/`, or its skills). That repository is paused for Codex. Do not run `codex-workflows`.
<!-- agent-skills-pause:end -->

## Tests

Use only end-to-end tests for new or replacement test coverage going forward.
Exercise real user workflows through public entry points and assert observable
results and persisted state. Use isolated temporary projects and deterministic
local external-service fixtures when needed; do not require live or paid services.

Do not add unit tests, source-text assertions, documentation phrase checks, or
tests that only assert values constructed by their own fixtures. Extend an
existing end-to-end workflow when it covers the behavior adequately.

Existing useful focused tests may still be run. Do not delete coverage merely
because it is not end-to-end; replace its useful guarantees with end-to-end
coverage before removing it.
