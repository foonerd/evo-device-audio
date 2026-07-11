#!/usr/bin/env bash
# dist/release/harness/run-primitive.sh
#
# Validation harness runner. Executes one install/reset primitive
# end-to-end on a target rig via SSH, captures the signed evidence
# file the rig-side evo-install.sh writes, and stores it under
# dist/release/evidence/<version>/<arch>/<primitive>.toml.
#
# The dist/release/preflight-cut.sh script later verifies these
# evidence files at release-cut time.
#
# One primitive per invocation. The orchestrator (run-all.sh)
# fans out across primitives and arches by invoking this script
# repeatedly.
#
# Usage:
#   dist/release/harness/run-primitive.sh \
#       --primitive p1_full_initial_setup \
#       --arch aarch64-unknown-linux-gnu \
#       --rig-host <ip-or-hostname> \
#       --rig-user <ssh-user> \
#       --version v0.1.13 \
#       --bundle-url <url-to-signed-bundle> \
#       --signing-key <path-to-ed25519-private-pem> \
#       [--evidence-out <path>]
#
# Every argument except --evidence-out is required.

set -euo pipefail

PRIMITIVE=""
ARCH=""
RIG_HOST=""
RIG_USER=""
VERSION=""
BUNDLE_URL=""
SIGNING_KEY=""
EVIDENCE_OUT=""

usage() {
    cat <<EOF >&2
Usage: $(basename "$0") \\
    --primitive <p1_full_initial_setup|p2_full_wipe_and_reinstall|p3_config_wipe_preserving_music|p4_user_data_full_vacuum> \\
    --arch <triple> \\
    --rig-host <host> \\
    --rig-user <user> \\
    --version <version> \\
    --bundle-url <url> \\
    --signing-key <path> \\
    [--evidence-out <path>]

Executes one install/reset primitive on the named rig via SSH,
captures the signed evidence record, stores it locally.
EOF
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --primitive)     PRIMITIVE="$2"; shift 2 ;;
        --arch)          ARCH="$2"; shift 2 ;;
        --rig-host)      RIG_HOST="$2"; shift 2 ;;
        --rig-user)      RIG_USER="$2"; shift 2 ;;
        --version)       VERSION="$2"; shift 2 ;;
        --bundle-url)    BUNDLE_URL="$2"; shift 2 ;;
        --signing-key)   SIGNING_KEY="$2"; shift 2 ;;
        --evidence-out)  EVIDENCE_OUT="$2"; shift 2 ;;
        -h|--help)       usage ;;
        *)               echo "unknown argument: $1" >&2; usage ;;
    esac
done

[[ -z "${PRIMITIVE}" ]]   && { echo "--primitive required" >&2; exit 1; }
[[ -z "${ARCH}" ]]        && { echo "--arch required" >&2; exit 1; }
[[ -z "${RIG_HOST}" ]]    && { echo "--rig-host required" >&2; exit 1; }
[[ -z "${RIG_USER}" ]]    && { echo "--rig-user required" >&2; exit 1; }
[[ -z "${VERSION}" ]]     && { echo "--version required" >&2; exit 1; }
[[ -z "${BUNDLE_URL}" ]]  && { echo "--bundle-url required" >&2; exit 1; }
[[ -z "${SIGNING_KEY}" ]] && { echo "--signing-key required" >&2; exit 1; }
[[ -r "${SIGNING_KEY}" ]] || { echo "signing key not readable: ${SIGNING_KEY}" >&2; exit 1; }

# Map primitive id → installer mode.
MODE=""
case "${PRIMITIVE}" in
    p1_full_initial_setup)            MODE="install" ;;
    p2_full_wipe_and_reinstall)       MODE="reinstall" ;;
    p3_config_wipe_preserving_music)  MODE="wipe-config" ;;
    p4_user_data_full_vacuum)         MODE="wipe-user-data" ;;
    *) echo "unknown primitive: ${PRIMITIVE}" >&2; exit 1 ;;
esac

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
if [[ -z "${EVIDENCE_OUT}" ]]; then
    EVIDENCE_OUT="${REPO_ROOT}/dist/release/evidence/${VERSION}/${ARCH}/${PRIMITIVE}.toml"
fi

echo "run-primitive: ${PRIMITIVE} on ${RIG_USER}@${RIG_HOST} (arch ${ARCH})"
echo "run-primitive: mode ${MODE}"
echo "run-primitive: evidence-out ${EVIDENCE_OUT}"

# Stage the signing key on the rig in a work dir.
REMOTE_WORK="/tmp/evo-harness-$$"
REMOTE_KEY="${REMOTE_WORK}/signing.key"
REMOTE_EVIDENCE="${REMOTE_WORK}/evidence.toml"

trap 'ssh -o BatchMode=yes "${RIG_USER}@${RIG_HOST}" "rm -rf ${REMOTE_WORK}" 2>/dev/null || true' EXIT

ssh -o BatchMode=yes -o ConnectTimeout=10 "${RIG_USER}@${RIG_HOST}" "mkdir -p ${REMOTE_WORK} && chmod 700 ${REMOTE_WORK}"
scp -o BatchMode=yes "${SIGNING_KEY}" "${RIG_USER}@${RIG_HOST}:${REMOTE_KEY}"
ssh -o BatchMode=yes "${RIG_USER}@${RIG_HOST}" "chmod 400 ${REMOTE_KEY}"

# Fetch the installer to the rig.
REMOTE_INSTALLER="${REMOTE_WORK}/evo-install.sh"
INSTALLER_URL="${INSTALLER_URL:-https://raw.githubusercontent.com/foonerd/evo-device-audio/main/dist/scripts/evo-install.sh}"
echo "run-primitive: fetching installer from ${INSTALLER_URL}"
ssh -o BatchMode=yes "${RIG_USER}@${RIG_HOST}" "curl -fsSL --retry 3 --retry-delay 2 -o ${REMOTE_INSTALLER} ${INSTALLER_URL} && chmod +x ${REMOTE_INSTALLER}"

# Run the primitive with the signing key + explicit evidence output
# path. sudo needed for install actions.
echo "run-primitive: executing primitive on rig"
ssh -o BatchMode=yes "${RIG_USER}@${RIG_HOST}" "sudo EVO_ACCEPTANCE_SIGNING_KEY=${REMOTE_KEY} EVO_INSTALL_EVIDENCE_OUT=${REMOTE_EVIDENCE} ${REMOTE_INSTALLER} --mode=${MODE} --bundle-url=${BUNDLE_URL} 2>&1" | sed 's/^/  [rig] /'

# Retrieve the signed evidence file.
install -d "$(dirname "${EVIDENCE_OUT}")"
scp -o BatchMode=yes "${RIG_USER}@${RIG_HOST}:${REMOTE_EVIDENCE}" "${EVIDENCE_OUT}"
echo "run-primitive: evidence retrieved → ${EVIDENCE_OUT}"

# Sanity-check the evidence file has a signature (not the unsigned
# placeholder).
if grep -q 'UNSIGNED_SIGNING_ERROR' "${EVIDENCE_OUT}"; then
    echo "run-primitive: REFUSE — evidence is unsigned (signing failed on rig)" >&2
    exit 2
fi
if ! grep -q '^ed25519_b64 = "' "${EVIDENCE_OUT}"; then
    echo "run-primitive: REFUSE — evidence has no signature block" >&2
    exit 2
fi

echo "run-primitive: OK. signed evidence written to ${EVIDENCE_OUT}."
