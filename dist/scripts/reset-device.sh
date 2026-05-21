#!/usr/bin/env bash
# reset-device.sh — wipe evo state on a target so a fresh
# deploy-distribution.sh invocation lands on a known-clean
# substrate.
#
# Routine operator action between distribution upgrades that
# change bundle shape (manifest, signed-bytes layout, on-disk
# schema). Reusable for diagnosis of stale-bundle drift,
# debugging admission paths, and validating a fresh-install
# code path on hardware that has accumulated state from
# prior runs.
#
# Three modes — pick the narrowest one that clears the
# problem:
#
#   --bundles  Wipe /opt/evo/plugins/ only. For "stale shape
#              under new manifest" cases. Steward binary,
#              state dir, /etc/evo/, sudoers, systemd unit +
#              drop-ins all preserved. deploy-distribution.sh
#              re-populates bundles on next run; no other
#              substrate is disturbed. Fastest mode; safe
#              for production rigs that need a clean
#              redeploy without losing operator state.
#
#   --soft     (default) Wipe bundles + runtime state. Adds
#              /var/lib/evo/ contents to the bundle wipe:
#              witness chains, durable subject state,
#              persisted GroupStore / RoleStore /
#              TrustLedger, credentials, plugin state dirs,
#              wizard plans. /etc/evo/ (operator config,
#              trust roots, wizard config) + sudoers +
#              systemd unit preserved. After this mode the
#              device is back to "first-boot empty" without
#              re-running bootstrap.sh. deploy-distribution
#              repopulates bundles; the steward regenerates
#              the chain, the witness signing key, the
#              bootstrap token on next start.
#
#   --full     Bare-to-prototype-installed. Everything --soft
#              wipes plus the steward binary, the catalogue,
#              all /etc/evo/ content, all evo sudoers
#              entries, all evo.service drop-ins, and the
#              UI assets. The framework systemd unit
#              (/etc/systemd/system/evo.service) is left in
#              place because prototype-install.sh owns it.
#              Recovery path: re-run bootstrap.sh (which
#              re-creates /etc/evo/ + sudoers + drop-ins)
#              then deploy-distribution.sh. Use when a
#              distribution upgrade changed the operator-
#              config shape or trust roots.
#
# Pre-flight always:
#
#   - SSH reachability (-o BatchMode=yes -o ConnectTimeout=5).
#   - Target identifies as an evo host: /opt/evo exists AND
#     /etc/systemd/system/evo.service exists. Refusing on
#     a non-evo host is the difference between "reset the
#     evo install" and "wipe random directories" — the
#     pre-flight is the safety property.
#
# Confirmation:
#
#   Interactive prompt before any wipe unless --yes is
#   supplied. For CI / scripted multi-rig runs, --yes
#   bypasses the prompt.
#
# Usage:
#
#   dist/scripts/reset-device.sh <TARGET_HOST> <TARGET_USER> \
#       [--bundles | --soft | --full] [--yes]
#
# Exit codes:
#   0 — wipe completed cleanly.
#   1 — operator error (wrong invocation, ssh refused,
#       target not an evo host, operator declined the
#       confirmation prompt).
#   2 — wipe error (a step on the target returned non-zero).

set -eo pipefail

usage() {
    cat >&2 <<'USAGE'
usage: dist/scripts/reset-device.sh <target-host> <target-user> [--bundles|--soft|--full] [--yes]

modes:
  --bundles  Wipe /opt/evo/plugins/ only. Operator state preserved.
  --soft     (default) Wipe bundles + /var/lib/evo/ runtime state.
             Configs, trust roots, sudoers, systemd drop-ins preserved.
  --full     Bare-to-prototype-installed. Adds /opt/evo/bin + catalogue
             + /etc/evo/* + evo sudoers + evo.service drop-ins to the
             wipe. Requires bootstrap.sh re-run on the target after.

flags:
  --yes      Skip the interactive confirmation prompt.

example:
  dist/scripts/reset-device.sh host.lan <service-user> --soft
  dist/scripts/reset-device.sh host.lan <service-user> --full --yes
USAGE
}

if [[ $# -lt 2 ]]; then
    usage
    exit 1
fi

TARGET_HOST="$1"
TARGET_USER="$2"
shift 2

MODE="--soft"
ASSUME_YES=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --bundles|--soft|--full) MODE="$1"; shift ;;
        --yes) ASSUME_YES=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *)
            echo "FAIL: unknown argument: $1" >&2
            usage
            exit 1
            ;;
    esac
done

SSH_TARGET="${TARGET_USER}@${TARGET_HOST}"

echo "=== reset-device.sh ==="
echo "Target: ${SSH_TARGET}"
echo "Mode:   ${MODE}"
echo

# ----------------------------------------------------------
# [0/3] Pre-flight: target reachable + identifies as evo
# ----------------------------------------------------------
echo "[0/3] pre-flight ..."
if ! ssh -o BatchMode=yes -o ConnectTimeout=5 "${SSH_TARGET}" \
        "test -d /opt/evo && test -f /etc/systemd/system/evo.service" \
        >/dev/null 2>&1; then
    echo "FAIL: target pre-flight failed." >&2
    echo "      Either ssh is unreachable, or the host does not look" >&2
    echo "      like an evo install (/opt/evo or /etc/systemd/system/evo.service missing)." >&2
    echo "      Refusing to wipe a non-evo host." >&2
    exit 1
fi
echo "  ok (target identified as an evo host)"
echo

# ----------------------------------------------------------
# [1/3] Confirmation
# ----------------------------------------------------------
case "${MODE}" in
    --bundles)
        WIPE_DESCRIPTION="OOP plugin bundles under /opt/evo/plugins/. Operator state, configs, trust roots, sudoers, and systemd drop-ins are preserved."
        ;;
    --soft)
        WIPE_DESCRIPTION="OOP plugin bundles + /var/lib/evo/ runtime state (witness chains, durable subject state, GroupStore/RoleStore/TrustLedger, credentials, plugin state dirs, wizard plans). Configs in /etc/evo/, trust roots, sudoers, and systemd drop-ins are preserved."
        ;;
    --full)
        WIPE_DESCRIPTION="OOP plugin bundles + /var/lib/evo/ runtime state + /opt/evo/bin + /opt/evo/catalogue + /opt/evo/ui + /etc/evo/* (plugins.d, trust.d, wizard, client_acl.toml, evo.toml, mpd.conf) + /etc/sudoers.d/evo-* + every drop-in under /etc/systemd/system/evo.service.d/. The framework systemd unit /etc/systemd/system/evo.service is preserved; bootstrap.sh must re-run after this mode."
        ;;
esac

if [[ "${ASSUME_YES}" -ne 1 ]]; then
    echo "About to wipe on ${SSH_TARGET}:"
    echo "  ${WIPE_DESCRIPTION}"
    echo
    read -r -p "Proceed? [y/N] " ANSWER
    if [[ "${ANSWER}" != "y" && "${ANSWER}" != "Y" ]]; then
        echo "Aborted." >&2
        exit 1
    fi
else
    echo "[1/3] confirmation skipped (--yes)"
    echo "  ${WIPE_DESCRIPTION}"
fi
echo

# ----------------------------------------------------------
# [2/3] Stop steward + wipe per mode
# ----------------------------------------------------------
echo "[2/3] stop steward + wipe ..."

# Build the wipe script that runs on the target. Each wipe
# step uses `rm -rf -- <path>/* <path>/.[!.]*` rather than
# `rm -rf <path>` so the parent directory's ownership and
# mode survive (the directory itself is recreated below if
# the mode demands it). Missing paths are tolerated (set +e
# inside the wipe block) so re-runs are idempotent.
WIPE_SCRIPT_HEAD="
set -e
sudo -n systemctl stop evo 2>/dev/null || true
"

WIPE_BUNDLES="
if [ -d /opt/evo/plugins ]; then
    sudo -n find /opt/evo/plugins -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
fi
"

WIPE_STATE="
if [ -d /var/lib/evo ]; then
    sudo -n find /var/lib/evo -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
fi
"

WIPE_BINARY="
if [ -d /opt/evo/bin ]; then
    sudo -n find /opt/evo/bin -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
fi
if [ -d /opt/evo/catalogue ]; then
    sudo -n find /opt/evo/catalogue -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
fi
if [ -d /opt/evo/ui ]; then
    sudo -n find /opt/evo/ui -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
fi
"

WIPE_CONFIG="
if [ -d /etc/evo ]; then
    sudo -n find /etc/evo -mindepth 1 -maxdepth 1 -exec rm -rf -- {} +
fi
sudo -n find /etc/sudoers.d -maxdepth 1 -name 'evo-*' -exec rm -f -- {} +
if [ -d /etc/systemd/system/evo.service.d ]; then
    sudo -n find /etc/systemd/system/evo.service.d -mindepth 1 -maxdepth 1 -exec rm -f -- {} +
fi
sudo -n systemctl daemon-reload
"

case "${MODE}" in
    --bundles) WIPE_BODY="${WIPE_BUNDLES}" ;;
    --soft)    WIPE_BODY="${WIPE_BUNDLES}${WIPE_STATE}" ;;
    --full)    WIPE_BODY="${WIPE_BUNDLES}${WIPE_STATE}${WIPE_BINARY}${WIPE_CONFIG}" ;;
esac

if ! ssh "${SSH_TARGET}" "${WIPE_SCRIPT_HEAD}${WIPE_BODY}"; then
    echo "FAIL: wipe step returned non-zero on target" >&2
    exit 2
fi
echo "  ok"
echo

# ----------------------------------------------------------
# [3/3] Verify the wipe + log next-step guidance
# ----------------------------------------------------------
echo "[3/3] verify ..."

case "${MODE}" in
    --bundles)
        REMAINING=$(ssh "${SSH_TARGET}" "ls -1 /opt/evo/plugins 2>/dev/null | wc -l")
        if [[ "${REMAINING}" -ne 0 ]]; then
            echo "  [WARN] /opt/evo/plugins not empty (${REMAINING} entries remain)"
        else
            echo "  [ok]  /opt/evo/plugins empty"
        fi
        ;;
    --soft)
        BUNDLE_REMAINING=$(ssh "${SSH_TARGET}" "ls -1 /opt/evo/plugins 2>/dev/null | wc -l")
        STATE_REMAINING=$(ssh "${SSH_TARGET}" "ls -1 /var/lib/evo 2>/dev/null | wc -l")
        if [[ "${BUNDLE_REMAINING}" -ne 0 ]]; then
            echo "  [WARN] /opt/evo/plugins not empty (${BUNDLE_REMAINING} entries remain)"
        else
            echo "  [ok]  /opt/evo/plugins empty"
        fi
        if [[ "${STATE_REMAINING}" -ne 0 ]]; then
            echo "  [WARN] /var/lib/evo not empty (${STATE_REMAINING} entries remain)"
        else
            echo "  [ok]  /var/lib/evo empty"
        fi
        ;;
    --full)
        BUNDLE_REMAINING=$(ssh "${SSH_TARGET}" "ls -1 /opt/evo/plugins 2>/dev/null | wc -l")
        STATE_REMAINING=$(ssh "${SSH_TARGET}" "ls -1 /var/lib/evo 2>/dev/null | wc -l")
        BIN_REMAINING=$(ssh "${SSH_TARGET}" "ls -1 /opt/evo/bin 2>/dev/null | wc -l")
        CONFIG_REMAINING=$(ssh "${SSH_TARGET}" "ls -1 /etc/evo 2>/dev/null | wc -l")
        SUDO_REMAINING=$(ssh "${SSH_TARGET}" "ls -1 /etc/sudoers.d/evo-* 2>/dev/null | wc -l")
        DROPIN_REMAINING=$(ssh "${SSH_TARGET}" "ls -1 /etc/systemd/system/evo.service.d 2>/dev/null | wc -l")
        UNIT_PRESENT=$(ssh "${SSH_TARGET}" "test -f /etc/systemd/system/evo.service && echo yes || echo no")
        if [[ "${BUNDLE_REMAINING}" -ne 0 ]]; then
            echo "  [WARN] /opt/evo/plugins not empty (${BUNDLE_REMAINING} entries remain)"
        else
            echo "  [ok]  /opt/evo/plugins empty"
        fi
        if [[ "${STATE_REMAINING}" -ne 0 ]]; then
            echo "  [WARN] /var/lib/evo not empty (${STATE_REMAINING} entries remain)"
        else
            echo "  [ok]  /var/lib/evo empty"
        fi
        if [[ "${BIN_REMAINING}" -ne 0 ]]; then
            echo "  [WARN] /opt/evo/bin not empty (${BIN_REMAINING} entries remain)"
        else
            echo "  [ok]  /opt/evo/bin empty"
        fi
        if [[ "${CONFIG_REMAINING}" -ne 0 ]]; then
            echo "  [WARN] /etc/evo not empty (${CONFIG_REMAINING} entries remain)"
        else
            echo "  [ok]  /etc/evo empty"
        fi
        if [[ "${SUDO_REMAINING}" -ne 0 ]]; then
            echo "  [WARN] /etc/sudoers.d/ still has ${SUDO_REMAINING} evo-* entries"
        else
            echo "  [ok]  /etc/sudoers.d/ free of evo-* entries"
        fi
        if [[ "${DROPIN_REMAINING}" -ne 0 ]]; then
            echo "  [WARN] /etc/systemd/system/evo.service.d not empty (${DROPIN_REMAINING} entries remain)"
        else
            echo "  [ok]  /etc/systemd/system/evo.service.d empty"
        fi
        if [[ "${UNIT_PRESENT}" != "yes" ]]; then
            echo "  [WARN] /etc/systemd/system/evo.service missing (expected to remain)"
        else
            echo "  [ok]  /etc/systemd/system/evo.service preserved"
        fi
        ;;
esac

echo
echo "=== reset-device.sh complete ==="
case "${MODE}" in
    --bundles)
        echo "Next step: dist/scripts/deploy-distribution.sh ${TARGET_HOST} ${TARGET_USER} <triple>"
        ;;
    --soft)
        echo "Next step: dist/scripts/deploy-distribution.sh ${TARGET_HOST} ${TARGET_USER} <triple>"
        echo "Steward will regenerate first-boot state (witness signing key,"
        echo "bootstrap token, projection caches) on next start."
        ;;
    --full)
        echo "Next steps (in order):"
        echo "  1. dist/scripts/bootstrap.sh   (re-creates /etc/evo/, sudoers, drop-ins)"
        echo "  2. dist/scripts/deploy-distribution.sh ${TARGET_HOST} ${TARGET_USER} <triple>"
        ;;
esac
