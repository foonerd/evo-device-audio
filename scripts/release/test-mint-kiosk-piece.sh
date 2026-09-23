#!/usr/bin/env bash
# test-mint-kiosk-piece.sh — refuse paths for the audio-train
# kiosk mint. Does not cross-build and does not write artefacts.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
MINT="${ROOT}/scripts/release/mint-kiosk-piece.sh"
KIOSK_ENG="${KIOSK_ENG_SRC:-$(cd "${ROOT}/.." && pwd)/evo-kiosk-eng}"

fail() { printf 'test-mint FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'test-mint OK: %s\n' "$*" >&2; }

expect_rc() {
    local want="$1" label="$2"
    shift 2
    set +e
    "$@" >/tmp/mint-test.out 2>/tmp/mint-test.err
    local rc=$?
    set -e
    [[ "${rc}" -eq "${want}" ]] || fail "${label}: rc=${rc} want ${want}: $(cat /tmp/mint-test.err)"
    pass "${label} rc=${want}"
}

# No args
expect_rc 2 "no args" bash "${MINT}"

# Missing key
unset EVO_PLUGIN_SIGNING_KEY || true
expect_rc 2 "missing key" bash "${MINT}" --kiosk-src "${KIOSK_ENG}" --out-dir /tmp/mint-kiosk-out

# Key present, source missing
KEY="$(mktemp)"
echo test-key > "${KEY}"
export EVO_PLUGIN_SIGNING_KEY="${KEY}"
expect_rc 2 "missing source" bash "${MINT}" --kiosk-src /tmp/no-such-kiosk-src --out-dir /tmp/mint-kiosk-out

# Source exists but has no mint scripts (empty tree)
EMPTY="$(mktemp -d)"
expect_rc 2 "source without Cross.toml" bash "${MINT}" --kiosk-src "${EMPTY}" --out-dir /tmp/mint-kiosk-out
mkdir -p "${EMPTY}/scripts/release"
: > "${EMPTY}/Cross.toml"
expect_rc 2 "source without cross-build" bash "${MINT}" --kiosk-src "${EMPTY}" --out-dir /tmp/mint-kiosk-out

# Sibling kiosk-eng is a lab layout, not a CI layout. When the
# tree is present, require the two scripts this mint calls.
# When it is absent, skip — refuse paths above already ran.
if [[ -d "${KIOSK_ENG}" ]]; then
    [[ -f "${KIOSK_ENG}/scripts/release/cross-build.sh" ]] \
        || fail "kiosk-eng present but cross-build.sh is missing"
    [[ -f "${KIOSK_ENG}/scripts/release/stage-kiosk-piece.sh" ]] \
        || fail "kiosk-eng present but stage-kiosk-piece.sh is missing"
    pass "kiosk-eng neighbour scripts present"
else
    pass "kiosk-eng sibling absent — neighbour script check skipped"
fi

rm -f "${KEY}"
rm -rf "${EMPTY}"
printf 'test-mint: refuse paths passed\n' >&2
