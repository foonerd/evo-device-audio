#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Just a Nerd
#
# overlay-staged-plugins.sh — put the already-staged plugin
# bundles and the USB wrapper onto one box, in one invocation.
#
# This is NOT a distribution deploy. `deploy-distribution.sh`
# builds and ships the whole distribution and requires passwordless
# sudo on the target; this script ships artefacts that were built
# and signed in an earlier sitting, and takes privilege the way the
# fleet actually grants it — an interactive sudo over a terminal
# the operator is sitting at. It adds no sudoers rule and needs no
# NOPASSWD.
#
# What it moves, per box:
#
#   /opt/evo/plugins/<plugin>/{manifest.toml,manifest.sig,
#                              plugin.bin,privileges.yaml}
#   /usr/local/bin/evo-usb-mount          (0755 root:root)
#
# then restarts `evo` so the steward re-admits the overlaid
# plugins.
#
# Usage:
#
#   dist/scripts/overlay-staged-plugins.sh <ipv4> [ssh-user]
#
#   The login comes from the second argument or $EVO_SSH_USER.
#
#   One address per invocation, deliberately: an overlay is a
#   per-box decision and a loop over the fleet hides which box
#   refused. Addresses only — a hostname that resolves somewhere
#   unintended is exactly the mistake this refuses to make.
#
# Exit codes:
#   0  overlay landed and was verified
#   1  bad invocation / local staging missing
#   2  target unreachable or not an evo box
#   3  transfer or verification failed — nothing was activated
#   4  no terminal for the sudo prompt

set -euo pipefail

PLUGINS=(
    "org.evoframework.playback.mpd"
    "org.evoframework.storage.usb"
    "org.evoframework.network.shares"
)
WRAPPER_DEST="/usr/local/bin/evo-usb-mount"

if [[ $# -lt 1 || $# -gt 2 ]]; then
    echo "usage: $0 <ipv4> [ssh-user]" >&2
    echo "       ssh-user may instead come from \$EVO_SSH_USER." >&2
    exit 1
fi
TARGET_IP="$1"
if [[ ! "${TARGET_IP}" =~ ^([0-9]{1,3}\.){3}[0-9]{1,3}$ ]]; then
    echo "FAIL: '${TARGET_IP}' is not an IPv4 address." >&2
    echo "      This script takes addresses, not hostnames." >&2
    exit 1
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# No account is baked in. The operator names the login, or sets
# EVO_SSH_USER; a default here would be one site's account shipped
# to every other site.
SSH_USER="${2:-${EVO_SSH_USER:-}}"
if [[ -z "${SSH_USER}" ]]; then
    echo "FAIL: no ssh user given." >&2
    echo "      Pass it as the second argument or set EVO_SSH_USER." >&2
    exit 1
fi
SSH_TARGET="${SSH_USER}@${TARGET_IP}"

# Nothing staged at all is an operator mistake, not a target
# problem — say so before reaching for the network.
if ! compgen -G "${REPO_ROOT}/target/*/release/bundles" >/dev/null; then
    echo "FAIL: no staged bundles under ${REPO_ROOT}/target/*/release/bundles." >&2
    echo "      Build and sign them before overlaying." >&2
    exit 1
fi

echo "=== overlay-staged-plugins.sh ==="
echo "Target:  ${SSH_TARGET}"
echo "Staged:  ${REPO_ROOT}/target/<triple>/release/bundles"
echo

# --- [1/6] target reachable, and an evo box -------------------
echo "[1/6] pre-flight ..."
ssh -o BatchMode=yes -o ConnectTimeout=5 "${SSH_TARGET}" 'true' \
    || { echo "  FAIL: keyless SSH to ${TARGET_IP} refused." >&2; exit 2; }
ssh "${SSH_TARGET}" 'test -d /opt/evo/plugins' \
    || { echo "  FAIL: /opt/evo/plugins missing — not an evo box." >&2; exit 2; }
ssh "${SSH_TARGET}" 'test -f /etc/systemd/system/evo.service' \
    || { echo "  FAIL: evo.service not installed on ${TARGET_IP}." >&2; exit 2; }

ARCH="$(ssh -o BatchMode=yes "${SSH_TARGET}" 'uname -m' 2>/dev/null || true)"
case "${ARCH}" in
    aarch64) TRIPLE="aarch64-unknown-linux-gnu" ;;
    x86_64)  TRIPLE="x86_64-unknown-linux-gnu" ;;
    "")      echo "  FAIL: could not read the target's arch." >&2; exit 2 ;;
    *)       echo "  FAIL: unsupported arch '${ARCH}'." >&2; exit 2 ;;
esac
echo "  ok (${ARCH} -> ${TRIPLE})"

# --- [2/6] the staged artefacts are present and signed --------
echo "[2/6] staged artefacts ..."
STAGE="${REPO_ROOT}/target/${TRIPLE}/release/bundles"
if [[ ! -d "${STAGE}" ]]; then
    echo "  FAIL: nothing staged for ${TRIPLE} at ${STAGE}." >&2
    echo "        Build and sign the bundles before overlaying." >&2
    exit 1
fi
for plugin in "${PLUGINS[@]}"; do
    for f in manifest.toml manifest.sig plugin.bin privileges.yaml; do
        if [[ ! -f "${STAGE}/${plugin}/${f}" ]]; then
            echo "  FAIL: ${STAGE}/${plugin}/${f} missing." >&2
            echo "        An unsigned or partial bundle is not overlaid." >&2
            exit 1
        fi
    done
done
WRAPPER_SRC="${STAGE}/wrapper/evo-usb-mount"
[[ -f "${WRAPPER_SRC}" ]] \
    || { echo "  FAIL: ${WRAPPER_SRC} missing." >&2; exit 1; }
echo "  ok (${#PLUGINS[@]} bundles + wrapper)"

# --- [3/6] a terminal for the sudo prompt ---------------------
# The fleet has no passwordless sudo and this script does not add
# any. The operator's own sudo is used over a terminal; with no
# terminal the prompt would go nowhere, so refuse here rather than
# hang after the copy.
if [[ ! -t 0 ]]; then
    echo "[3/6] FAIL: no terminal on which sudo could prompt." >&2
    echo "      Re-run from an interactive terminal." >&2
    exit 4
fi
echo "[3/6] terminal present for the sudo prompt ... ok"

# --- [4/6] copy to a staging dir on the box (unprivileged) ----
REMOTE_TMP="/tmp/evo-overlay.$$"
echo "[4/6] copy to ${TARGET_IP}:${REMOTE_TMP} ..."
ssh "${SSH_TARGET}" "rm -rf '${REMOTE_TMP}' && mkdir -p '${REMOTE_TMP}'"
for plugin in "${PLUGINS[@]}"; do
    scp -q -r "${STAGE}/${plugin}" "${SSH_TARGET}:${REMOTE_TMP}/" \
        || { echo "  FAIL: copying ${plugin} failed." >&2; exit 3; }
done
scp -q "${WRAPPER_SRC}" "${SSH_TARGET}:${REMOTE_TMP}/evo-usb-mount" \
    || { echo "  FAIL: copying the wrapper failed." >&2; exit 3; }
echo "  ok"

# --- [5/6] verify the copy before anything is activated -------
# Nothing is installed on the strength of a transfer that did not
# land. Compare content, not presence.
echo "[5/6] verify the copy ..."
verify_one() {
    local local_path="$1" remote_path="$2"
    local want have
    want="$(sha256sum "${local_path}" | cut -d' ' -f1)"
    have="$(ssh "${SSH_TARGET}" "sha256sum '${remote_path}' 2>/dev/null | cut -d' ' -f1")"
    if [[ "${want}" != "${have}" ]]; then
        echo "  FAIL: ${remote_path} does not match what was sent." >&2
        echo "        sent ${want}, landed ${have:-<nothing>}" >&2
        echo "        Nothing has been installed; the box is untouched." >&2
        exit 3
    fi
}
for plugin in "${PLUGINS[@]}"; do
    for f in manifest.toml manifest.sig plugin.bin privileges.yaml; do
        verify_one "${STAGE}/${plugin}/${f}" "${REMOTE_TMP}/${plugin}/${f}"
    done
done
verify_one "${WRAPPER_SRC}" "${REMOTE_TMP}/evo-usb-mount"
echo "  ok (every file matches)"

# --- [6/6] install + restart, under one interactive sudo ------
# One `ssh -t` session so the operator is asked for their password
# once per box rather than once per file.
echo "[6/6] install and restart evo (sudo will prompt) ..."
INSTALL_SCRIPT="set -e"
for plugin in "${PLUGINS[@]}"; do
    INSTALL_SCRIPT+="
sudo mkdir -p /opt/evo/plugins/${plugin}
sudo install -m 0644 -o root -g root ${REMOTE_TMP}/${plugin}/manifest.toml    /opt/evo/plugins/${plugin}/manifest.toml
sudo install -m 0644 -o root -g root ${REMOTE_TMP}/${plugin}/manifest.sig     /opt/evo/plugins/${plugin}/manifest.sig
sudo install -m 0755 -o root -g root ${REMOTE_TMP}/${plugin}/plugin.bin       /opt/evo/plugins/${plugin}/plugin.bin
sudo install -m 0644 -o root -g root ${REMOTE_TMP}/${plugin}/privileges.yaml  /opt/evo/plugins/${plugin}/privileges.yaml"
done
INSTALL_SCRIPT+="
sudo install -m 0755 -o root -g root ${REMOTE_TMP}/evo-usb-mount ${WRAPPER_DEST}
rm -rf ${REMOTE_TMP}
sudo systemctl restart evo"
ssh -t "${SSH_TARGET}" "${INSTALL_SCRIPT}" \
    || { echo "  FAIL: install or restart refused on ${TARGET_IP}." >&2; exit 3; }
echo "  ok"
echo

# --- report what is actually on the box -----------------------
echo "=== on ${TARGET_IP} now ==="
for plugin in "${PLUGINS[@]}"; do
    printf '  %-34s %s\n' "${plugin}" \
        "$(ssh "${SSH_TARGET}" "sha256sum /opt/evo/plugins/${plugin}/plugin.bin | cut -d' ' -f1")"
done
printf '  %-34s %s\n' "${WRAPPER_DEST}" \
    "$(ssh "${SSH_TARGET}" "sha256sum ${WRAPPER_DEST} | cut -d' ' -f1")"
printf '  %-34s %s\n' "evo" \
    "$(ssh "${SSH_TARGET}" 'systemctl is-active evo; echo -n "  MainPID "; systemctl show -p MainPID --value evo' | tr '\n' ' ')"
