#!/usr/bin/env bash
#
# write-distribution-pointer.sh — project GitHub Release assets into
# bundles/distribution/<version>.toml.
#
# The Release is the store. This script never reads compose tempdir
# hashes. A second compose cannot reproduce a published sha256
# because compose embeds built_at_utc in the tarball.
#
# gh --jq takes one expression and does not accept jq --arg.
# All filters use jq(1) on a --json assets blob.
#
# Usage (live):
#   scripts/release/write-distribution-pointer.sh \
#     --tag v0.1.13.0 --version 0.1.13 --out PATH
#
# Usage (fixture, no network):
#   scripts/release/write-distribution-pointer.sh \
#     --tag v0.1.13.0 --version 0.1.13 --out PATH \
#     --assets-json testdata/.../assets.json \
#     --sidecar-dir testdata/...
#
set -euo pipefail

TAG=""
VERSION=""
OUT=""
REPO="foonerd/evo-device-audio-artefacts"
ASSETS_JSON_FILE=""
SIDECAR_DIR=""
PROTECTED_RELEASE="v0.1.13"

TRIPLES=(
    x86_64-unknown-linux-gnu
    aarch64-unknown-linux-gnu
    armv7-unknown-linux-gnueabihf
)

die() { printf '[distribution-pointer] FAIL: %s\n' "$*" >&2; exit 3; }
log() { printf '[distribution-pointer] %s\n' "$*" >&2; }

usage() {
    cat >&2 <<'EOF'
usage: scripts/release/write-distribution-pointer.sh \
    --tag v0.1.13.0 --version 0.1.13 --out PATH \
    [--repo foonerd/evo-device-audio-artefacts] \
    [--assets-json FILE] \
    [--sidecar-dir DIR]
EOF
    exit 2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag)          TAG="$2"; shift 2 ;;
        --version)      VERSION="$2"; shift 2 ;;
        --out)          OUT="$2"; shift 2 ;;
        --repo)         REPO="$2"; shift 2 ;;
        --assets-json)  ASSETS_JSON_FILE="$2"; shift 2 ;;
        --sidecar-dir)  SIDECAR_DIR="$2"; shift 2 ;;
        -h|--help)      usage ;;
        *) die "unknown argument: $1" ;;
    esac
done

[[ -n "${TAG}" && -n "${VERSION}" && -n "${OUT}" ]] || usage
[[ "${TAG}" != "${PROTECTED_RELEASE}" ]] || die "refusing to project GitHub Release ${PROTECTED_RELEASE}"

if [[ -n "${ASSETS_JSON_FILE}" ]]; then
    [[ -f "${ASSETS_JSON_FILE}" ]] || die "assets json missing: ${ASSETS_JSON_FILE}"
    ASSETS_JSON="$(cat "${ASSETS_JSON_FILE}")"
else
    command -v gh >/dev/null || die "gh is required unless --assets-json is set"
    ASSETS_JSON="$(gh release view "${TAG}" --repo "${REPO}" --json assets)"
fi
[[ -n "${ASSETS_JSON}" ]] || die "empty assets json"

if [[ -z "${SIDECAR_DIR}" ]]; then
    SIDECAR_DIR="$(mktemp -d -t evo-pointer-sidecars-XXXXXX)"
    trap 'rm -rf "${SIDECAR_DIR}"' EXIT
    command -v gh >/dev/null || die "gh is required to download sidecars"
    for triple in "${TRIPLES[@]}"; do
        base="evo-device-audio-${triple}-${VERSION}.tar.gz"
        gh release download "${TAG}" \
            --repo "${REPO}" \
            --pattern "${base}.sha256" \
            --dir "${SIDECAR_DIR}"
    done
fi
[[ -d "${SIDECAR_DIR}" ]] || die "sidecar dir missing: ${SIDECAR_DIR}"

asset_name_exists() {
    local name="$1"
    printf '%s' "${ASSETS_JSON}" | jq -e --arg n "${name}" \
        '.assets[] | select(.name==$n)' >/dev/null
}

asset_size() {
    local name="$1"
    printf '%s' "${ASSETS_JSON}" | jq -r --arg n "${name}" \
        '.assets[] | select(.name==$n) | .size'
}

sidecar_sha() {
    local base="$1"
    local side="${SIDECAR_DIR}/${base}.sha256"
    local sha
    [[ -s "${side}" ]] || die "sidecar missing: ${side}"
    sha="$(awk '{print $1}' "${side}")"
    [[ "${sha}" =~ ^[0-9a-f]{64}$ ]] || die "sidecar sha256 is not 64 hex: ${side}"
    printf '%s\n' "${sha}"
}

DOWNLOAD_BASE="https://github.com/${REPO}/releases/download/${TAG}"

{
    echo "schema_version = 0"
    echo "kind = \"distribution-bundle\""
    echo "version = \"${VERSION}\""
    echo "cut = \"${TAG}\""
    echo "publisher = \"org.evoframework\""
    echo "release_url = \"https://github.com/${REPO}/releases/tag/${TAG}\""
    echo "download_base = \"${DOWNLOAD_BASE}\""
    echo
    for triple in "${TRIPLES[@]}"; do
        base="evo-device-audio-${triple}-${VERSION}.tar.gz"
        asset_name_exists "${base}" || die "store missing ${base}"
        asset_name_exists "${base}.sig" || die "store missing ${base}.sig"
        asset_name_exists "${base}.sha256" || die "store missing ${base}.sha256"
        size="$(asset_size "${base}")"
        [[ "${size}" =~ ^[0-9]+$ ]] || die "store size missing for ${base}"
        sha="$(sidecar_sha "${base}")"
        echo "[[assets]]"
        echo "target = \"${triple}\""
        echo "name = \"${base}\""
        echo "sha256 = \"${sha}\""
        echo "size = ${size}"
        echo
        log "projected ${base} sha256=${sha} size=${size}"
    done
} > "${OUT}"

log "wrote ${OUT}"
