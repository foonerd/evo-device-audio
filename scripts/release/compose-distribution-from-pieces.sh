#!/usr/bin/env bash
# compose-distribution-from-pieces.sh — first-boot tarball from
# already-published piece slots. Does not cargo/cross/npm.
# Missing pin = fail closed.
#
# Usage:
#   EVO_PLUGIN_SIGNING_KEY=/path/to.pem \
#   scripts/release/compose-distribution-from-pieces.sh \
#     --target TRIPLE \
#     --artefacts-repo PATH \
#     --bundle-dir PATH \
#     [--pins PATH] \
#     [--cargo-version 0.1.13]

set -euo pipefail

TARGET=""
ARTEFACTS=""
BUNDLE_DIR=""
PINS=""
CARGO_VERSION=""

TRIPLES_OK=(
    x86_64-unknown-linux-gnu
    aarch64-unknown-linux-gnu
    armv7-unknown-linux-gnueabihf
)

log() { printf '[compose] %s\n' "$*" >&2; }
die() { printf '[compose] FAIL: %s\n' "$*" >&2; exit 3; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target)          TARGET="$2"; shift 2 ;;
        --artefacts-repo)  ARTEFACTS="$2"; shift 2 ;;
        --bundle-dir)      BUNDLE_DIR="$2"; shift 2 ;;
        --pins)            PINS="$2"; shift 2 ;;
        --cargo-version)   CARGO_VERSION="$2"; shift 2 ;;
        *) die "unknown argument: $1" ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PINS="${PINS:-${SCRIPT_DIR}/piece-pins.toml}"

[[ -n "${TARGET}" && -n "${ARTEFACTS}" && -n "${BUNDLE_DIR}" ]] \
    || die "usage: --target --artefacts-repo --bundle-dir"
[[ -d "${ARTEFACTS}" ]] || die "artefacts repo missing: ${ARTEFACTS}"
[[ -f "${PINS}" ]] || die "pins file missing: ${PINS}"
[[ -n "${EVO_PLUGIN_SIGNING_KEY:-}" && -r "${EVO_PLUGIN_SIGNING_KEY}" ]] \
    || die "EVO_PLUGIN_SIGNING_KEY unset"

ok=0
for t in "${TRIPLES_OK[@]}"; do
    [[ "${t}" == "${TARGET}" ]] && ok=1
done
[[ "${ok}" -eq 1 ]] || die "unsupported target ${TARGET}"

if [[ -z "${CARGO_VERSION}" ]]; then
    CARGO_VERSION="$(awk -F'"' '/^version =/ {print $2; exit}' "${REPO_ROOT}/Cargo.toml")"
fi

pin() {
    local name="$1"
    awk -F'"' -v k="${name}" '$0 ~ "^"k" *=" {print $2; exit}' "${PINS}"
}

require_file() {
    [[ -f "$1" ]] || die "missing piece file: $1 (mint this piece before bake)"
}

unpack_tree() {
    local tgz="$1" dest="$2"
    require_file "${tgz}"
    local tmp
    tmp="$(mktemp -d -t evo-unpack-XXXXXX)"
    tar -xzf "${tgz}" -C "${tmp}"
    local top
    top="$(find "${tmp}" -mindepth 1 -maxdepth 1 -type d | head -1)"
    [[ -n "${top}" ]] || die "tree.tar.gz has no top-level directory: ${tgz}"
    mkdir -p "${dest}"
    cp -a "${top}/." "${dest}/"
    rm -rf "${tmp}"
}

STEWARD_VER="$(pin evo-device-audio)"
SHELL_VER="$(pin evo-ui-shell)"
RUNTIME_VER="$(pin evo-ui-runtime)"
KIOSK_VER="$(pin evo-kiosk)"
BOOT_VER="$(pin evo-device-boot)"
DIST_VER="$(pin evo-device-audio-dist)"
[[ -n "${STEWARD_VER}${SHELL_VER}${RUNTIME_VER}${KIOSK_VER}${BOOT_VER}${DIST_VER}" ]] \
    || die "piece-pins.toml is incomplete"

# shellcheck source=../../dist/scripts/lib/oop-plugins.sh
source "${REPO_ROOT}/dist/scripts/lib/oop-plugins.sh"

STAGE="$(mktemp -d -t evo-compose-XXXXXX)"
trap 'rm -rf "${STAGE}"' EXIT

log "target ${TARGET} workspace ${CARGO_VERSION}"
log "pins steward=${STEWARD_VER} ui-shell=${SHELL_VER} ui-runtime=${RUNTIME_VER} kiosk=${KIOSK_VER} boot=${BOOT_VER} dist=${DIST_VER}"

# Dist tree + plugin overlays
unpack_tree "${ARTEFACTS}/bundles/evo-device-audio-dist/${DIST_VER}/tree.tar.gz" "${STAGE}/_distpiece"
cp -a "${STAGE}/_distpiece/dist" "${STAGE}/dist"
if [[ -d "${STAGE}/_distpiece/plugin-overlays" ]]; then
    mkdir -p "${STAGE}/plugins"
    for ov in "${STAGE}/_distpiece/plugin-overlays"/*; do
        [[ -d "${ov}" ]] || continue
        name="$(basename "${ov}")"
        mkdir -p "${STAGE}/plugins/${name}"
        cp -a "${ov}/." "${STAGE}/plugins/${name}/"
    done
fi

# Steward
steward_src="${ARTEFACTS}/binaries/evo-device-audio/${STEWARD_VER}/${TARGET}/evo-device-audio"
require_file "${steward_src}"
mkdir -p "${STAGE}/bin"
install -m 0755 "${steward_src}" "${STAGE}/bin/evo-device-audio"

# Plugins (already signed)
for entry in "${OOP_PLUGINS[@]}"; do
    IFS=':' read -r p_name _ _ _ <<< "${entry}"
    p_ver="$(awk -F'"' '/^version *=/{print $2; exit}' "${REPO_ROOT}/plugins/${p_name}/manifest.oop.toml")"
    [[ -n "${p_ver}" ]] || die "no version in plugins/${p_name}/manifest.oop.toml"
    tgz="${ARTEFACTS}/bundles/${p_name}/${TARGET}/${p_name}-${p_ver}-${TARGET}.tar.gz"
    require_file "${tgz}"
    tmp="$(mktemp -d -t evo-plug-XXXXXX)"
    tar -xzf "${tgz}" -C "${tmp}"
    top="$(find "${tmp}" -mindepth 1 -maxdepth 1 -type d | head -1)"
    mkdir -p "${STAGE}/plugins/${p_name}"
    cp -a "${top}/." "${STAGE}/plugins/${p_name}/"
    rm -rf "${tmp}"
    log "plugin ${p_name} ${p_ver}"
done

# UI shell
unpack_tree "${ARTEFACTS}/bundles/evo-ui-shell/${SHELL_VER}/tree.tar.gz" "${STAGE}/ui"
if [[ -f "${STAGE}/dist/ui-overlay/setup.html" ]]; then
    install -m 0644 "${STAGE}/dist/ui-overlay/setup.html" "${STAGE}/ui/setup.html"
fi

# UI runtime
rt="${ARTEFACTS}/binaries/evo-ui-runtime/${RUNTIME_VER}/${TARGET}/evo-ui-runtime"
unit="${ARTEFACTS}/binaries/evo-ui-runtime/${RUNTIME_VER}/${TARGET}/evo-ui.service.in"
require_file "${rt}"
require_file "${unit}"
mkdir -p "${STAGE}/ui-runtime"
install -m 0755 "${rt}" "${STAGE}/ui-runtime/evo-ui-runtime"
install -m 0644 "${unit}" "${STAGE}/ui-runtime/evo-ui.service.in"

# Boot (arch-independent)
unpack_tree "${ARTEFACTS}/bundles/evo-device-boot/${BOOT_VER}/tree.tar.gz" \
    "${STAGE}/layers/evo-device-boot"

# Kiosk: prefer per-target slot, else arch-independent
kiosk_tgz="${ARTEFACTS}/bundles/evo-kiosk/${KIOSK_VER}/${TARGET}/tree.tar.gz"
if [[ ! -f "${kiosk_tgz}" ]]; then
    kiosk_tgz="${ARTEFACTS}/bundles/evo-kiosk/${KIOSK_VER}/tree.tar.gz"
fi
unpack_tree "${kiosk_tgz}" "${STAGE}/layers/evo-kiosk-eng"

{
    echo "schema_version = 1"
    echo "bundle_kind = \"evo-device-audio\""
    echo "version = \"${CARGO_VERSION}\""
    echo "architecture = \"${TARGET}\""
    echo "composed_from_pieces = true"
    echo "built_at_utc = \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\""
    echo ""
    echo "[plugins]"
    for entry in "${OOP_PLUGINS[@]}"; do
        IFS=':' read -r p_name _ _ _ <<< "${entry}"
        echo "${p_name} = true"
    done
} > "${STAGE}/bundle-manifest.toml"

mkdir -p "${BUNDLE_DIR}"
BUNDLE_BASE="evo-device-audio-${TARGET}-${CARGO_VERSION}"
BUNDLE_TGZ="${BUNDLE_DIR}/${BUNDLE_BASE}.tar.gz"
BUNDLE_SIG="${BUNDLE_TGZ}.sig"

tar -C "${STAGE}" \
    --sort=name \
    --mtime='2026-01-01 00:00:00 UTC' \
    --owner=0 --group=0 --numeric-owner \
    -czf "${BUNDLE_TGZ}" \
    .
openssl pkeyutl -sign \
    -inkey "${EVO_PLUGIN_SIGNING_KEY}" -rawin \
    -in "${BUNDLE_TGZ}" \
    -out "${BUNDLE_SIG}"

log "wrote ${BUNDLE_TGZ} ($(wc -c < "${BUNDLE_TGZ}") bytes)"
