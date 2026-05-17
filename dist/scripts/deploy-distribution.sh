#!/usr/bin/env bash
# deploy-distribution.sh — cross-build the evo-device-audio
# steward for a target architecture, ship it to the target host,
# restart the unit, and verify the boot trace.
#
# Idempotent in shape: re-running re-builds + re-ships + restarts
# cleanly. The previous binary is preserved as
# `evo-device-audio.prev` for single-step rollback after deploy.
#
# Prerequisites on the target host:
#   - `prototype-install.sh` (framework-tier base install) has
#     run; `/etc/evo/`, `/opt/evo/`, the trust keys + the
#     framework's systemd unit are in place.
#   - `bootstrap.sh` (distribution-tier install) has run;
#     `/etc/sudoers.d/evo-*`, `/etc/systemd/system/evo.service.d/`
#     drop-ins (including `exec-start.conf` that overrides the
#     framework `ExecStart` to point at `evo-device-audio`), and
#     `/etc/evo/plugins.d/` configs are in place.
#
# Prerequisites on the dev box:
#   - `cross` (https://github.com/cross-rs/cross) installed
#     OR the host's stable toolchain + matching cross-link
#     blocks in `.cargo/config.toml` (the rig's existing setup).
#   - SSH access to the target as the operator-configured
#     service user.
#
# Usage:
#
#   scripts/deploy-distribution.sh <TARGET_HOST> <TARGET_USER> <TARGET_TRIPLE>
#
#   All three arguments are required; the script does not bake
#   in defaults to avoid accidentally deploying to a previously-
#   used host.
#
#   TARGET_HOST   — IP or hostname of the target reachable via
#                   ssh from the dev box.
#   TARGET_USER   — operator-configured service user on the
#                   target (matches the user `bootstrap.sh`
#                   resolved at install time).
#   TARGET_TRIPLE — Rust target triple; the compiled binary's
#                   architecture must match the target's CPU
#                   family. Common values:
#                     aarch64-unknown-linux-gnu  (64-bit ARM)
#                     x86_64-unknown-linux-gnu   (64-bit x86)
#                     armv7-unknown-linux-gnueabihf  (32-bit ARM)
#
# Exit codes:
#   0 — build + deploy + restart succeeded; service active.
#   1 — operator error (wrong invocation, ssh refused, missing
#       prerequisite on target).
#   2 — build error (cross build failed; previous deploy untouched).
#   3 — deploy error (scp / install failed; previous binary remains).
#   4 — verify error (service did not become active within budget).

set -euo pipefail

if [[ $# -lt 3 ]]; then
    echo "usage: $0 <target-host> <target-user> <target-triple>" >&2
    echo "example: $0 host.lan <service-user> aarch64-unknown-linux-gnu" >&2
    exit 1
fi

TARGET_HOST="$1"
TARGET_USER="$2"
TARGET_TRIPLE="$3"
SSH_TARGET="${TARGET_USER}@${TARGET_HOST}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# The distribution binary's canonical install path on the target.
TARGET_BIN_PATH="/opt/evo/bin/evo-device-audio"
TARGET_BIN_PREV="/opt/evo/bin/evo-device-audio.prev"

# The crate that produces the bundled steward binary. Same name
# as the binary it builds.
DIST_CRATE="evo-device-audio-distribution"
DIST_BIN="evo-device-audio"

echo "=== deploy-distribution.sh ==="
echo "Target:        ${SSH_TARGET}"
echo "Target triple: ${TARGET_TRIPLE}"
echo "Repo root:     ${REPO_ROOT}"
echo

# ----------------------------------------------------------
# [0/5] Pre-flight: target reachable + base install present.
# ----------------------------------------------------------
echo "[0/5] pre-flight on target ..."
if ! ssh -o BatchMode=yes -o ConnectTimeout=5 "${SSH_TARGET}" "
    set -e
    test -d /opt/evo/bin || {
        echo 'FAIL: /opt/evo/bin missing on target (run prototype-install.sh first)' >&2
        exit 1
    }
    test -f /etc/systemd/system/evo.service || {
        echo 'FAIL: evo.service unit missing on target (run prototype-install.sh first)' >&2
        exit 1
    }
    test -f /etc/systemd/system/evo.service.d/exec-start.conf || {
        echo 'WARN: exec-start.conf drop-in missing; systemd may launch the framework default binary on next restart (run bootstrap.sh)' >&2
    }
" >/dev/null 2>&1; then
    echo "FAIL: target pre-flight failed; cannot continue" >&2
    exit 1
fi
echo "  ok"
echo

# ----------------------------------------------------------
# [1/5] Cross-build the steward binary.
# ----------------------------------------------------------
echo "[1/5] cross-build ${DIST_CRATE} for ${TARGET_TRIPLE} ..."
cd "${REPO_ROOT}"

CROSS_HELPER="${REPO_ROOT}/scripts/cross-build.sh"
if [[ -x "${CROSS_HELPER}" ]]; then
    # Use the repo's cross helper (handles path-dep mount
    # workaround, etc.). Forward the standard release +
    # alsa-substrate build profile.
    if ! "${CROSS_HELPER}" "${TARGET_TRIPLE}" --release \
            --features alsa-substrate -p "${DIST_CRATE}" >/dev/null 2>&1; then
        echo "FAIL: cross-build via scripts/cross-build.sh exited non-zero" >&2
        exit 2
    fi
else
    # Fallback: direct cargo build with --target.
    if ! cargo build --release --target "${TARGET_TRIPLE}" \
            --features alsa-substrate -p "${DIST_CRATE}" >/dev/null 2>&1; then
        echo "FAIL: cargo build --target ${TARGET_TRIPLE} exited non-zero" >&2
        exit 2
    fi
fi

LOCAL_BIN="${REPO_ROOT}/target/${TARGET_TRIPLE}/release/${DIST_BIN}"
if [[ ! -x "${LOCAL_BIN}" ]]; then
    echo "FAIL: expected binary missing at ${LOCAL_BIN}" >&2
    exit 2
fi
echo "  ok (binary: ${LOCAL_BIN})"
echo

# ----------------------------------------------------------
# [2/5] Stop the steward; preserve previous binary.
# ----------------------------------------------------------
echo "[2/5] stop steward + preserve previous binary as evo-device-audio.prev ..."
if ! ssh "${SSH_TARGET}" "
    set -e
    sudo -n systemctl stop evo || true
    if [ -f ${TARGET_BIN_PATH} ]; then
        sudo -n cp ${TARGET_BIN_PATH} ${TARGET_BIN_PREV}
    fi
"; then
    echo "FAIL: could not stop steward on target" >&2
    exit 3
fi
echo "  ok"
echo

# ----------------------------------------------------------
# [3/5] scp the new binary + install in place.
# ----------------------------------------------------------
echo "[3/5] scp + install fresh binary ..."
TMP_REMOTE="/tmp/evo-device-audio.deploy.$$"
if ! scp -q "${LOCAL_BIN}" "${SSH_TARGET}:${TMP_REMOTE}"; then
    echo "FAIL: scp to target failed" >&2
    exit 3
fi
if ! ssh "${SSH_TARGET}" "
    set -e
    sudo -n install -m 0755 -o root -g root ${TMP_REMOTE} ${TARGET_BIN_PATH}
    rm -f ${TMP_REMOTE}
"; then
    echo "FAIL: install on target failed" >&2
    exit 3
fi
echo "  ok"
echo

# ----------------------------------------------------------
# [4/5] Start steward.
# ----------------------------------------------------------
echo "[4/5] start steward ..."
if ! ssh "${SSH_TARGET}" 'sudo -n systemctl start evo'; then
    echo "FAIL: systemctl start evo returned non-zero" >&2
    exit 4
fi
# Brief settle window before the verify probe.
sleep 3
echo "  ok"
echo

# ----------------------------------------------------------
# [5/5] Verify service is active + steward emitted ready.
# ----------------------------------------------------------
echo "[5/5] verify ..."
ACTIVE_STATE="$(ssh "${SSH_TARGET}" 'systemctl is-active evo' 2>/dev/null || true)"
if [[ "${ACTIVE_STATE}" != "active" ]]; then
    echo "FAIL: evo.service is not active (state=${ACTIVE_STATE})" >&2
    echo "      check 'journalctl -u evo --no-pager -n 80' on the target" >&2
    exit 4
fi
echo "  [ok]  evo.service active"

READY_HITS="$(ssh "${SSH_TARGET}" \
    'sudo -n journalctl -u evo --since "30 seconds ago" --no-pager 2>&1 \
        | grep -cE "evo ready|server listening|fast path listening"')"
if [[ "${READY_HITS}" -ge 1 ]]; then
    echo "  [ok]  steward emitted ready / listening signals (${READY_HITS} matching lines)"
else
    echo "  [WARN] no ready / listening signal in the last 30 s of evo journal"
    echo "         (check 'journalctl -u evo --no-pager -n 80' on the target)"
fi

echo
echo "=== deploy-distribution.sh complete ==="
echo "Binary deployed to ${SSH_TARGET}:${TARGET_BIN_PATH}"
echo "Previous binary preserved at ${SSH_TARGET}:${TARGET_BIN_PREV}"
echo "Rollback: ssh ${SSH_TARGET} 'sudo -n cp ${TARGET_BIN_PREV} ${TARGET_BIN_PATH} && sudo -n systemctl restart evo'"
