#!/usr/bin/env bash
# mint-kiosk-piece.sh — mint the evo-kiosk piece on the audio train.
#
# Compose refuses a first-boot box whose kiosk piece has no
# program. This script is the mint that produces one: it runs
# the kiosk tree's cross-build, then that tree's
# stage-kiosk-piece.sh, which places the built program into the
# packed piece. The program comes from the cross-build output,
# not from a working-tree layer/binaries copy.
#
# Does not write foonerd/evo-device-audio-artefacts. Upload is
# a separate publish job and is not implied by a successful mint.
#
# Usage:
#   EVO_PLUGIN_SIGNING_KEY=/path/to.pem \
#   scripts/release/mint-kiosk-piece.sh \
#     --kiosk-src PATH \
#     --out-dir PATH \
#     [--version VER]
#
# --kiosk-src must be a tree that carries Cross.toml,
# scripts/release/cross-build.sh, and
# scripts/release/stage-kiosk-piece.sh (evo-kiosk-eng).
#
# --version defaults to crates/kiosk-browser/Cargo.toml in
# that tree.
#
# Exit codes:
#   0 — piece packed; every required triple has a program.
#   2 — usage / precondition failure.
#   3 — cross-build or stage refused (incomplete program set).

set -euo pipefail

KIOSK_SRC=""
OUT=""
VERSION=""

log() { printf '[mint-kiosk] %s\n' "$*" >&2; }
die() { printf '[mint-kiosk] FAIL: %s\n' "$*" >&2; exit 2; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --kiosk-src) KIOSK_SRC="$2"; shift 2 ;;
        --out-dir)   OUT="$2"; shift 2 ;;
        --version)   VERSION="$2"; shift 2 ;;
        *) die "unknown argument: $1" ;;
    esac
done

[[ -n "${KIOSK_SRC}" && -n "${OUT}" ]] \
    || die "usage: mint-kiosk-piece.sh --kiosk-src PATH --out-dir PATH [--version VER]"

[[ -n "${EVO_PLUGIN_SIGNING_KEY:-}" && -r "${EVO_PLUGIN_SIGNING_KEY}" ]] \
    || die "EVO_PLUGIN_SIGNING_KEY unset or unreadable"

[[ -d "${KIOSK_SRC}" ]] || die "kiosk source is not a directory: ${KIOSK_SRC}"
[[ -f "${KIOSK_SRC}/Cross.toml" ]] || die "kiosk source has no Cross.toml: ${KIOSK_SRC}"

CROSS_BUILD="${KIOSK_SRC}/scripts/release/cross-build.sh"
STAGE="${KIOSK_SRC}/scripts/release/stage-kiosk-piece.sh"
[[ -f "${CROSS_BUILD}" ]] || die "missing ${CROSS_BUILD}"
[[ -f "${STAGE}" ]] || die "missing ${STAGE}"

if [[ -z "${VERSION}" ]]; then
    VERSION="$(awk -F'"' '/^version *=/{print $2; exit}' \
        "${KIOSK_SRC}/crates/kiosk-browser/Cargo.toml")"
fi
[[ -n "${VERSION}" ]] || die "could not read kiosk-browser version"

log "kiosk-src ${KIOSK_SRC}"
log "version ${VERSION}"
log "out-dir ${OUT}"

log "cross-build every required triple"
if ! bash "${CROSS_BUILD}"; then
    printf '[mint-kiosk] FAIL: cross-build refused\n' >&2
    exit 3
fi

mkdir -p "${OUT}"
log "stage and pack signed piece"
if ! bash "${STAGE}" --version "${VERSION}" --out-dir "${OUT}"; then
    printf '[mint-kiosk] FAIL: stage-kiosk-piece refused\n' >&2
    exit 3
fi

[[ -f "${OUT}/tree.tar.gz" ]] || die "stage wrote no ${OUT}/tree.tar.gz"
log "packed ${OUT}/tree.tar.gz ($(wc -c < "${OUT}/tree.tar.gz") bytes)"
