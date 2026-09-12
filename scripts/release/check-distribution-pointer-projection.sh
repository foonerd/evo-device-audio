#!/usr/bin/env bash
#
# Offline proof that the distribution pointer is a projection of
# the Release store. Does not dispatch CI. Does not mutate artefacts.
#
# Also records the GitHub CLI contract that caused the org-visible
# bake failure: `gh --jq` accepts one expression, not `jq --arg`.
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DATA="${ROOT}/testdata/distribution-pointer"
WRITER="${ROOT}/write-distribution-pointer.sh"
OUT="$(mktemp -t evo-pointer-out-XXXXXX.toml)"
trap 'rm -f "${OUT}"' EXIT

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }
pass() { printf 'OK: %s\n' "$*" >&2; }

[[ -x "${WRITER}" ]] || fail "${WRITER} is not executable"

# --- contract: gh --jq does not take --arg ---
if gh release view --help >/dev/null 2>&1; then
    set +e
    gh_err="$(gh release view v0.1.13.0 --repo foonerd/evo-device-audio-artefacts \
        --json assets --jq --arg n 'x' '.assets[0].name' 2>&1)"
    gh_rc=$?
    set -e
    if [[ "${gh_rc}" -eq 0 ]]; then
        fail "gh --jq --arg unexpectedly succeeded; do not use it"
    fi
    printf '%s' "${gh_err}" | grep -q 'accepts at most 1 arg' \
        || fail "gh --jq --arg error text changed: ${gh_err}"
    pass "gh --jq --arg is rejected (the shipped bake bug)"
else
    pass "gh not in PATH; skipped live --jq contract check"
fi

# --- fixture: full store projects the known pointer ---
bash "${WRITER}" \
    --tag v0.1.13.0 \
    --version 0.1.13 \
    --out "${OUT}" \
    --assets-json "${DATA}/assets.json" \
    --sidecar-dir "${DATA}"
diff -u "${DATA}/expected.toml" "${OUT}" || fail "projection does not match expected.toml"
pass "fixture projection matches published sidecars and sizes"

# --- fixture: missing armv7 is fail-closed ---
set +e
miss_err="$(bash "${WRITER}" \
    --tag v0.1.13.0 \
    --version 0.1.13 \
    --out "${OUT}" \
    --assets-json "${DATA}/assets-missing-armv7.json" \
    --sidecar-dir "${DATA}" 2>&1)"
miss_rc=$?
set -e
[[ "${miss_rc}" -ne 0 ]] || fail "missing armv7 must fail closed"
printf '%s' "${miss_err}" | grep -q 'store missing evo-device-audio-armv7' \
    || fail "missing armv7 error unclear: ${miss_err}"
pass "missing armv7 triple fails closed"

# --- refuse projecting the protected Latest cut ---
set +e
prot_err="$(bash "${WRITER}" \
    --tag v0.1.13 \
    --version 0.1.13 \
    --out "${OUT}" \
    --assets-json "${DATA}/assets.json" \
    --sidecar-dir "${DATA}" 2>&1)"
prot_rc=$?
set -e
[[ "${prot_rc}" -ne 0 ]] || fail "must refuse tag v0.1.13"
printf '%s' "${prot_err}" | grep -q 'v0.1.13' \
    || fail "protected-release error unclear: ${prot_err}"
pass "refuses to project protected GitHub Release v0.1.13"

# --- artefacts commit: add before the first rebase ---
# The pointer write dirties the clone. `git pull --rebase` on that
# dirty tree is the 128 this check exists to keep closed.
COMMIT_BLOCK="$(
    awk '/Artefacts commit \(pointer only\)/,0' \
        "${ROOT}/publish-distribution-bundle.sh"
)"
printf '%s' "${COMMIT_BLOCK}" | grep -q 'git add' \
    || fail "pointer commit block must git add"
ADD_LINE="$(printf '%s\n' "${COMMIT_BLOCK}" | grep -n 'git add' | head -1 | cut -d: -f1)"
REBASE_LINE="$(printf '%s\n' "${COMMIT_BLOCK}" | grep -n 'pull --rebase' | head -1 | cut -d: -f1)"
COMMIT_LINE="$(printf '%s\n' "${COMMIT_BLOCK}" | grep -n 'git commit' | head -1 | cut -d: -f1)"
[[ -n "${ADD_LINE}" && -n "${REBASE_LINE}" && -n "${COMMIT_LINE}" ]] \
    || fail "pointer commit block missing add / commit / rebase"
[[ "${ADD_LINE}" -lt "${COMMIT_LINE}" && "${COMMIT_LINE}" -lt "${REBASE_LINE}" ]] \
    || fail "pointer commit must add, then commit, then rebase (got add=${ADD_LINE} commit=${COMMIT_LINE} rebase=${REBASE_LINE})"
pass "pointer commit adds and commits before rebase"

pass "distribution pointer projection checks"
