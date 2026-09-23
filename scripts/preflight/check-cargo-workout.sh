#!/usr/bin/env bash
#
# check-cargo-workout.sh — compulsory commit entry gate.
#
# Matches public audio CI (toolchain 1.85, no --locked) and adds
# the two steps that CI historically omitted and that therefore
# slipped: start from `cargo clean`, and rustdoc with
# RUSTDOCFLAGS='-D warnings'.
#
# Run from the workspace root before every commit. Exits 0 only
# when every step is clean. Do not #[allow] rustdoc or clippy
# to silence this gate.
#
# Usage:
#   scripts/preflight/check-cargo-workout.sh
#   scripts/preflight/check-cargo-workout.sh --locked   # cut / pin-flipped tree

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

TOOLCHAIN="${CARGO_TOOLCHAIN:-1.85}"
LOCKED=0
if [[ "${1:-}" == "--locked" ]]; then
    LOCKED=1
fi

lock_args=()
if [[ "${LOCKED}" -eq 1 ]]; then
    lock_args+=(--locked)
fi

log_step() { printf '\n[workout] %s\n' "$*" >&2; }
log_ok()   { printf '[workout] OK: %s\n' "$*" >&2; }
log_fail() { printf '[workout] FAIL: %s\n' "$*" >&2; }

unset CARGO_TARGET_DIR

log_step "1/5 cargo +${TOOLCHAIN} clean"
cargo "+${TOOLCHAIN}" clean
log_ok "clean"

log_step "2/5 cargo +${TOOLCHAIN} fmt --all -- --check"
if ! cargo "+${TOOLCHAIN}" fmt --all -- --check; then
    log_fail "fmt drift. Fix: cargo +${TOOLCHAIN} fmt --all"
    exit 1
fi
log_ok "fmt"

log_step "3/5 cargo +${TOOLCHAIN} clippy --workspace --all-targets ${lock_args[*]:-} -- -D warnings"
if ! cargo "+${TOOLCHAIN}" clippy --workspace --all-targets "${lock_args[@]}" -- -D warnings; then
    log_fail "clippy -D warnings"
    exit 1
fi
log_ok "clippy"

log_step "4/5 cargo +${TOOLCHAIN} test --workspace ${lock_args[*]:-}"
if ! cargo "+${TOOLCHAIN}" test --workspace "${lock_args[@]}"; then
    log_fail "tests"
    exit 1
fi
log_ok "test"

log_step "5/5 RUSTDOCFLAGS='-D warnings' cargo +${TOOLCHAIN} doc --workspace --no-deps ${lock_args[*]:-}"
if ! RUSTDOCFLAGS='-D warnings' cargo "+${TOOLCHAIN}" doc --workspace --no-deps "${lock_args[@]}"; then
    log_fail "rustdoc -D warnings (intra-doc links, private links, HTML, rustdoc lints)"
    exit 1
fi
log_ok "rustdoc"

printf '\n[workout] all five steps clean (toolchain +%s%s).\n' \
    "${TOOLCHAIN}" "$([[ ${LOCKED} -eq 1 ]] && echo ', --locked' || true)" >&2
