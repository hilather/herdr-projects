# Signed factory admission

`factory_admission` stays `off` until an operator runs `factory admission`. That command is the only production writer. The migration default is unchanged. `PREPARED_LAUNCH_DISPATCH_ENABLED` is unchanged. This documentation does not enable a project.

```sh
ssh-keygen -Y sign -f /path/to/owner-key -n admission@herdr-projects policy.json
herdr-projects factory admission PROJECT --enable --policy policy.json --signature policy.json.sig --evidence vertical-slice-manifest.json
herdr-projects factory admission PROJECT --disable --policy disable.json --signature disable.json.sig
```

`--enable` and `--disable` both verify the raw policy file under `admission@herdr-projects`. The verifier does not reserialize the file. The file must be at most 65,536 bytes. The signed document names `project_store` and `evidence_digest`, and it must match the pinned owner policy.

`--enable` also requires schema 30 or newer, a configured integration ref that exists and is not checked out, and an `--evidence` file whose SHA-256 equals `evidence_digest`. That file is a JSON manifest whose `vertical_slice` field is the string `pass`. A boolean `true` is not a pass. There is no compile-time bypass.

`--disable` uses the same signature check and does not read a manifest, so admission can be withdrawn while a ref is checked out. Schema older than 30 is refused for both actions.

A refusal leaves the column `off` and writes an authority denial with class `admission`. The library test setter is `#[cfg(test)]`, is not `pub`, and is not a CLI path.

Enabling the flag does not by itself clear a dependency blocker. A valid satisfaction while the flag is on is already reported without `admission_disabled`. That rule is unchanged.
