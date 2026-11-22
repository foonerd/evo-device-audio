#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Just a Nerd
#
# wait-for-piece-slots.sh — cut(V): poll artefacts until every pin
# and every plugin slot for this cut exists. Bake must not compose
# before this returns 0.
#
# Usage:
#   scripts/release/wait-for-piece-slots.sh \
#     --artefacts-repo PATH \
#     [--timeout-sec N] \
#     [--poll-sec N]
set -euo pipefail

ARTEFACTS_REPO=""
TIMEOUT_SEC=5400
POLL_SEC=30

usage() {
    cat >&2 <<'EOF'
usage: scripts/release/wait-for-piece-slots.sh \
    --artefacts-repo PATH \
    [--timeout-sec 5400] \
    [--poll-sec 30]
EOF
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --artefacts-repo) ARTEFACTS_REPO="$2"; shift 2 ;;
        --timeout-sec)    TIMEOUT_SEC="$2"; shift 2 ;;
        --poll-sec)       POLL_SEC="$2"; shift 2 ;;
        -h|--help)        usage ;;
        *) echo "unknown argument: $1" >&2; usage ;;
    esac
done

[[ -n "${ARTEFACTS_REPO}" ]] || usage
ARTEFACTS_REPO="$(cd "${ARTEFACTS_REPO}" && pwd)"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PINS="${SCRIPT_DIR}/piece-pins.toml"
[[ -f "${PINS}" ]] || { echo "missing ${PINS}" >&2; exit 2; }

CUT_VER="$(tr -d ' \t\r\n' < "${REPO_ROOT}/CUT_VERSION" 2>/dev/null || true)"
[[ -n "${CUT_VER}" ]] || { echo "CUT_VERSION missing or empty" >&2; exit 2; }
[[ "${CUT_VER}" != v* ]] || { echo "CUT_VERSION must not carry a leading v" >&2; exit 2; }

pin() {
    awk -F'"' -v k="$1" '$1 ~ "^"k"[[:space:]]*=" {print $2; exit}' "${PINS}"
}

STEWARD_VER="$(pin evo-device-audio)"
SHELL_VER="$(pin evo-ui-shell)"
RUNTIME_VER="$(pin evo-ui-runtime)"
KIOSK_VER="$(pin evo-kiosk)"
BOOT_VER="$(pin evo-device-boot)"
DIST_VER="$(pin evo-device-audio-dist)"
[[ -n "${STEWARD_VER}${SHELL_VER}${RUNTIME_VER}${KIOSK_VER}${BOOT_VER}${DIST_VER}" ]] \
    || { echo "piece-pins.toml incomplete" >&2; exit 2; }

# shellcheck source=../../dist/scripts/lib/oop-plugins.sh
source "${REPO_ROOT}/dist/scripts/lib/oop-plugins.sh"

TRIPLES=(
    x86_64-unknown-linux-gnu
    aarch64-unknown-linux-gnu
    armv7-unknown-linux-gnueabihf
)

required_paths() {
    local triple p_name
    for triple in "${TRIPLES[@]}"; do
        echo "binaries/evo-device-audio/${STEWARD_VER}/${triple}/evo-device-audio"
        echo "binaries/evo-ui-runtime/${RUNTIME_VER}/${triple}/evo-ui-runtime"
        echo "binaries/evo-ui-runtime/${RUNTIME_VER}/${triple}/evo-ui.service.in"
        for entry in "${OOP_PLUGINS[@]}"; do
            IFS=':' read -r p_name _ _ _ <<< "${entry}"
            echo "bundles/${p_name}/${triple}/${p_name}-${CUT_VER}-${triple}.tar.gz"
        done
    done
    echo "bundles/evo-ui-shell/${SHELL_VER}/tree.tar.gz"
    echo "bundles/evo-device-audio-dist/${DIST_VER}/tree.tar.gz"
    echo "bundles/evo-device-boot/${BOOT_VER}/tree.tar.gz"
}

kiosk_ok() {
    local triple
    if [[ -f "${ARTEFACTS_REPO}/bundles/evo-kiosk/${KIOSK_VER}/tree.tar.gz" ]]; then
        return 0
    fi
    for triple in "${TRIPLES[@]}"; do
        if [[ ! -f "${ARTEFACTS_REPO}/bundles/evo-kiosk/${KIOSK_VER}/${triple}/tree.tar.gz" ]]; then
            return 1
        fi
    done
    return 0
}

missing_list() {
    local path rel
    # Refresh artefacts tip so a late mint is visible.
    if [[ -d "${ARTEFACTS_REPO}/.git" || -f "${ARTEFACTS_REPO}/.git" ]]; then
        git -C "${ARTEFACTS_REPO}" fetch --quiet origin 2>/dev/null || true
        git -C "${ARTEFACTS_REPO}" pull --ff-only --quiet 2>/dev/null || true
    fi
    while IFS= read -r rel; do
        path="${ARTEFACTS_REPO}/${rel}"
        if [[ ! -e "${path}" ]]; then
            printf '%s\n' "${rel}"
        fi
    done < <(required_paths)
    if ! kiosk_ok; then
        printf 'bundles/evo-kiosk/%s/{tree.tar.gz|*/tree.tar.gz}\n' "${KIOSK_VER}"
    fi
}

echo "wait-for-piece-slots: cut=${CUT_VER} artefacts=${ARTEFACTS_REPO} timeout=${TIMEOUT_SEC}s"
deadline=$(( $(date +%s) + TIMEOUT_SEC ))
while true; do
    mapfile -t missing < <(missing_list)
    if [[ ${#missing[@]} -eq 0 ]]; then
        echo "wait-for-piece-slots: OK — every pin and plugin slot for ${CUT_VER} is present"
        exit 0
    fi
    now=$(date +%s)
    if (( now >= deadline )); then
        echo "wait-for-piece-slots: TIMEOUT after ${TIMEOUT_SEC}s; still missing:" >&2
        printf '  %s\n' "${missing[@]}" >&2
        exit 3
    fi
    echo "wait-for-piece-slots: ${#missing[@]} missing (e.g. ${missing[0]}); sleep ${POLL_SEC}s"
    sleep "${POLL_SEC}"
done
