#!/usr/bin/env bash
#
# pre-tag-check.sh — run the full pre-tag verification chain
# before the operator mints a release tag on foonerd/evo-device-audio.
#
# Mirrors the evo-core-eng discipline: five workspace gates plus
# two distribution-specific gates (catalogue-schemas alignment
# preflight + ADR-0134 build-time lint).
#
# Run this immediately before `git tag <release>`. Exits 0 only
# when every gate is clean.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

log_step() { printf '\n[pre-tag] %s\n' "$*" >&2; }
log_ok()   { printf '[pre-tag] OK: %s\n' "$*" >&2; }
log_fail() { printf '[pre-tag] FAIL: %s\n' "$*" >&2; }

# -------------------------------------------------------------
# Gate 1: cargo fmt --all -- --check
# -------------------------------------------------------------

log_step "Gate 1/7: cargo fmt --all -- --check"
if ! cargo fmt --all -- --check; then
    log_fail "cargo fmt drift detected"
    log_fail "Fix: cargo fmt --all"
    exit 1
fi
log_ok "fmt clean"

# -------------------------------------------------------------
# Gate 2: cargo clippy --workspace --all-targets --locked -- -D warnings
# -------------------------------------------------------------

log_step "Gate 2/7: cargo clippy --workspace --all-targets --locked -- -D warnings"
if ! cargo clippy --workspace --all-targets --locked -- -D warnings; then
    log_fail "clippy warnings (treated as errors)"
    exit 1
fi
log_ok "clippy clean"

# -------------------------------------------------------------
# Gate 3: cargo test --workspace --locked
# -------------------------------------------------------------

log_step "Gate 3/7: cargo test --workspace --locked"
if ! cargo test --workspace --locked; then
    log_fail "test failure"
    exit 1
fi
log_ok "tests pass"

# -------------------------------------------------------------
# Gate 4: public-leak grep
# -------------------------------------------------------------

log_step "Gate 4/7: scripts/preflight/check-public-leaks.sh"
if [[ -x "${REPO_ROOT}/scripts/preflight/check-public-leaks.sh" ]]; then
    if ! bash "${REPO_ROOT}/scripts/preflight/check-public-leaks.sh"; then
        log_fail "leak gate hit"
        exit 1
    fi
else
    log_fail "scripts/preflight/check-public-leaks.sh missing"
    exit 1
fi

# -------------------------------------------------------------
# Gate 5: catalogue-schemas alignment (R-009)
# -------------------------------------------------------------

log_step "Gate 5/7: scripts/preflight/check-catalogue-schemas-alignment.sh"
if [[ -x "${REPO_ROOT}/scripts/preflight/check-catalogue-schemas-alignment.sh" ]]; then
    if ! bash "${REPO_ROOT}/scripts/preflight/check-catalogue-schemas-alignment.sh"; then
        log_fail "catalogue-schemas alignment gate hit"
        log_fail "Fix: reconcile plugins/*/manifest.toml shelf/shape against foonerd/evo-catalogue-schemas"
        exit 1
    fi
else
    log_fail "scripts/preflight/check-catalogue-schemas-alignment.sh missing"
    exit 1
fi

# -------------------------------------------------------------
# Gate 6: ADR-0134 build-time lint
# -------------------------------------------------------------

log_step "Gate 6/7: dist/release/build-time-lint.sh"
if [[ -x "${REPO_ROOT}/dist/release/build-time-lint.sh" ]]; then
    if ! REPO_ROOT="${REPO_ROOT}" bash "${REPO_ROOT}/dist/release/build-time-lint.sh"; then
        log_fail "build-time-lint gate hit"
        exit 1
    fi
else
    log_fail "dist/release/build-time-lint.sh missing"
    exit 1
fi

# -------------------------------------------------------------
# Gate 7: workspace build
# -------------------------------------------------------------

log_step "Gate 7/7: cargo build --workspace --locked"
if ! cargo build --workspace --locked; then
    log_fail "workspace build failed"
    exit 1
fi
log_ok "workspace build clean"

# -------------------------------------------------------------
# All gates clean
# -------------------------------------------------------------

cat >&2 <<'BANNER'

[pre-tag] All seven gates clean. Ready for tag mint.

Next steps:
  1. Run dist/release/harness/run-all.sh to produce signed
     ADR-0134 evidence across every supported (primitive x arch)
     pair.
  2. Verify with dist/release/preflight-cut.sh.
  3. Mint the tag: v<MAJOR>.<MINOR>.<PATCH>[.<CLOSURE>][-<PRERELEASE>]
     Tag-format regex enforced at publish:
       ^v[0-9]+\.[0-9]+\.[0-9]+(\.[0-9]+)?(-[0-9A-Za-z.-]+)?$
  4. Run scripts/release/promote.sh to drive the
     eng -> public squash-and-scrub.
BANNER
