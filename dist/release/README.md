# dist/release/

Release-cut tooling: three scripts, one gated flow that keeps a release cut from advancing until signed evidence exists for every install / reset primitive on every supported architecture.

## Files

- `build-time-lint.sh` — build-time lint. Cross-checks structured sources (plugin manifests × dist/catalogue × dist/sudoers × known-target-binaries table). Refuses the release build on any contract violation.
- `harness/run-primitive.sh` — validation harness (one primitive). Executes one install / reset primitive end-to-end on a target rig via SSH, retrieves the signed evidence record.
- `harness/run-all.sh` — validation harness orchestrator. Drives `run-primitive.sh` across every `(primitive × arch)` pair the release cut needs.
- `preflight-cut.sh` — release-cut preflight. Consumes the signed evidence set and refuses to advance the cut if anything is missing, stale, unsigned, or post-condition-mismatched.

## Evidence directory

`dist/release/evidence/<version>/<arch>/<primitive>.toml` — signed evidence records the harness writes and the preflight verifies. This path is a runtime artefact tree; it is NOT checked in (added to `.gitignore`).

## Rig map

The harness reads a rig-map TOML file describing which physical rig hosts each supported architecture. This file lives OUT-OF-REPO (rig IPs / hostnames are internal and never checked in). Example shape:

```toml
[rigs.aarch64-unknown-linux-gnu]
host = "<host>"
user = "<user>"

[rigs.x86_64-unknown-linux-gnu]
host = "<host>"
user = "<user>"
```

## Signing key

`--signing-key <path>` is the ed25519 private PEM key. The public counterpart travels in the release-cut preflight. The signing key never enters the repo.

## Flow

```
build-time-lint.sh  →  (green)
     ↓
harness/run-all.sh  →  writes dist/release/evidence/<version>/<arch>/<primitive>.toml (signed)
     ↓
preflight-cut.sh    →  verifies evidence set
     ↓
scripts/release/promote.sh  →  cut proceeds if preflight passed
```
