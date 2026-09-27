# Task contract v3: required Git files

Version 3 adds a required `outputs` array of 1–64 declarations with exact
`path` and `kind: "git_file"`. Paths must be normalized, literal repository-relative
paths. Duplicate paths, glob patterns, directory declarations and `.git` components
are refused. Each output must be inside a declared write scope (an exact file or
an explicit directory prefix). A read scope does not authorize an output.

Versions 1 and 2 retain their signed byte compatibility and cannot declare outputs.
Version 2 still requires an exact barrier reference. Version 3 permits the initial
wave without a barrier; a supplied barrier uses the same exact reference validation
and runtime binding as version 2.

Result manifests must list each required output. This is a completeness check,
not evidence that the output exists. Before running the acceptance policy, the
verifier independently checks the materialized retained Git candidate. Every
required output must be a regular file with no symlink components. Missing files,
directories and symlink substitutions produce a rejected verification run with
`required_output_missing`, without a receipt or capacity release. The existing
policy and clean-tree checks still apply before acceptance.

The JSON vector and SHA-256 sidecar bind exact document bytes. This addition
implements output declarations and existence enforcement only. It does not claim
complete F2 decomposition, parent-deliverable coverage, planner-driven contract
production or live acceptance. Content correctness remains the responsibility of
the bound acceptance policy.

Explicit path scopes also constrain the full base-to-candidate Git diff, not only
required output files. Literal file paths and directory prefixes authorize writes;
read declarations and glob patterns do not. Rename source and destination paths
are both checked. A violation atomically records verifier rejection, feedback and
a cancellation request using the existing proof-before-release rules. Feedback
enters the existing replan queue. This verifies retained changes after execution;
it does not claim filesystem containment of a running worker.
