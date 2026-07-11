#!/usr/bin/env bash
# dist/release/harness/run-all.sh
#
# Validation harness orchestrator. Drives
# run-primitive.sh across every (primitive x arch) pair the release
# cut requires, so `dist/release/preflight-cut.sh` finds complete
# per-arch signed evidence for all four primitives.
#
# Reads a rig-map TOML file describing which physical rig hosts
# each supported architecture. Format:
#
#   [rigs.aarch64-unknown-linux-gnu]
#   host = "<host-or-ip>"
#   user = "<ssh-user>"
#
#   [rigs.x86_64-unknown-linux-gnu]
#   host = "<host-or-ip>"
#   user = "<ssh-user>"
#
# The rig-map lives OUT-OF-REPO (rig IPs / hostnames are internal
# and never checked in per project discipline).
#
# Usage:
#   dist/release/harness/run-all.sh \
#       --version v0.1.13 \
#       --bundle-url <url-to-signed-bundle> \
#       --signing-key <path-to-ed25519-private-pem> \
#       --rig-map <path-to-rig-map.toml>

set -euo pipefail

VERSION=""
BUNDLE_URL=""
SIGNING_KEY=""
RIG_MAP=""

usage() {
    cat <<EOF >&2
Usage: $(basename "$0") \\
    --version <version> \\
    --bundle-url <url> \\
    --signing-key <path> \\
    --rig-map <path>

Runs every install/reset primitive on every supported arch rig,
producing signed evidence for the release-cut preflight.
EOF
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)      VERSION="$2"; shift 2 ;;
        --bundle-url)   BUNDLE_URL="$2"; shift 2 ;;
        --signing-key)  SIGNING_KEY="$2"; shift 2 ;;
        --rig-map)      RIG_MAP="$2"; shift 2 ;;
        -h|--help)      usage ;;
        *)              echo "unknown argument: $1" >&2; usage ;;
    esac
done

[[ -z "${VERSION}" ]]     && { echo "--version required" >&2; exit 1; }
[[ -z "${BUNDLE_URL}" ]]  && { echo "--bundle-url required" >&2; exit 1; }
[[ -z "${SIGNING_KEY}" ]] && { echo "--signing-key required" >&2; exit 1; }
[[ -z "${RIG_MAP}" ]]     && { echo "--rig-map required" >&2; exit 1; }
[[ -r "${RIG_MAP}" ]]     || { echo "rig-map not readable: ${RIG_MAP}" >&2; exit 1; }

readonly PRIMITIVES=(
    "p1_full_initial_setup"
    "p2_full_wipe_and_reinstall"
    "p3_config_wipe_preserving_music"
    "p4_user_data_full_vacuum"
)

# Parse rig-map: extract every [rigs.<arch>] section's host + user.
declare -A RIG_HOST RIG_USER
current_arch=""
while IFS= read -r line; do
    if [[ "${line}" =~ ^\[rigs\.([^]]+)\]$ ]]; then
        current_arch="${BASH_REMATCH[1]}"
        continue
    fi
    [[ -z "${current_arch}" ]] && continue
    if [[ "${line}" =~ ^host[[:space:]]*=[[:space:]]*\"([^\"]+)\" ]]; then
        RIG_HOST["${current_arch}"]="${BASH_REMATCH[1]}"
    elif [[ "${line}" =~ ^user[[:space:]]*=[[:space:]]*\"([^\"]+)\" ]]; then
        RIG_USER["${current_arch}"]="${BASH_REMATCH[1]}"
    fi
done < "${RIG_MAP}"

if (( ${#RIG_HOST[@]} == 0 )); then
    echo "run-all: rig-map declared no rigs" >&2
    exit 1
fi

HERE="$(cd "$(dirname "$0")" && pwd)"
RUN_PRIMITIVE="${HERE}/run-primitive.sh"
[[ -x "${RUN_PRIMITIVE}" ]] || { echo "run-primitive.sh not executable: ${RUN_PRIMITIVE}" >&2; exit 1; }

FAILURES=0
for arch in "${!RIG_HOST[@]}"; do
    host="${RIG_HOST[${arch}]}"
    user="${RIG_USER[${arch}]:-}"
    if [[ -z "${user}" ]]; then
        echo "run-all: rig-map missing user for arch ${arch}" >&2
        FAILURES=$((FAILURES + 1))
        continue
    fi
    echo "=== ${arch} on ${user}@${host} ==="
    for primitive in "${PRIMITIVES[@]}"; do
        if "${RUN_PRIMITIVE}" \
            --primitive "${primitive}" \
            --arch "${arch}" \
            --rig-host "${host}" \
            --rig-user "${user}" \
            --version "${VERSION}" \
            --bundle-url "${BUNDLE_URL}" \
            --signing-key "${SIGNING_KEY}"; then
            echo "  ${primitive}: OK"
        else
            echo "  ${primitive}: FAIL"
            FAILURES=$((FAILURES + 1))
        fi
    done
done

echo
if (( FAILURES > 0 )); then
    echo "run-all: ${FAILURES} primitive-run failure(s). Evidence set incomplete." >&2
    exit 1
fi

echo "run-all: PASS. All primitives × arches produced signed evidence."
