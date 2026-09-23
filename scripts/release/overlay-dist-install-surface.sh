#!/usr/bin/env bash
# overlay-dist-install-surface.sh — put this repo's install
# primitives onto an unpacked dist tree.
#
# Usage:
#   overlay-dist-install-surface.sh --repo-root PATH --dest-dist PATH
#
# The frozen evo-device-audio-dist piece is a published
# snapshot. Playground bootstrap moves independently. Compose
# unpacks the piece, then this overlay, so the bundled
# bootstrap and the files it hard-requires are one generation.
# That is not a remint: alsa/, plugins.d/, keys/, catalogue/,
# and systemd drop-ins stay the piece.
#
# Overlay set (deliberate, not "all of dist/"):
#   dist/scripts/bootstrap.sh
#   dist/scripts/lib/          (chown-tree + the parsers bootstrap sources)
#   dist/bin/evo-rtc-wake
#   dist/sudoers.d/evo-rtc-wake.in
#   dist/systemd/evo.service
# Other sudoers templates stay the piece so a kiosk-privilege
# expansion cannot ride this row.

set -euo pipefail

REPO_ROOT=""
DEST_DIST=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo-root) REPO_ROOT="$2"; shift 2 ;;
        --dest-dist) DEST_DIST="$2"; shift 2 ;;
        *) echo "overlay-dist-install-surface: unknown argument: $1" >&2; exit 2 ;;
    esac
done

[[ -n "${REPO_ROOT}" && -n "${DEST_DIST}" ]] \
    || { echo "usage: overlay-dist-install-surface.sh --repo-root PATH --dest-dist PATH" >&2; exit 2; }

need() {
    [[ -e "$1" ]] || { echo "overlay-dist-install-surface: missing $1" >&2; exit 1; }
}

need "${REPO_ROOT}/dist/scripts/bootstrap.sh"
need "${REPO_ROOT}/dist/scripts/lib/chown-tree-same-fs.sh"
need "${REPO_ROOT}/dist/scripts/lib/chown-tenant-state-trees.sh"
need "${REPO_ROOT}/dist/bin/evo-rtc-wake"
need "${REPO_ROOT}/dist/sudoers.d/evo-rtc-wake.in"
need "${REPO_ROOT}/dist/systemd/evo.service"

mkdir -p "${DEST_DIST}/scripts/lib" \
    "${DEST_DIST}/bin" \
    "${DEST_DIST}/sudoers.d" \
    "${DEST_DIST}/systemd"

install -m 0755 "${REPO_ROOT}/dist/scripts/bootstrap.sh" \
    "${DEST_DIST}/scripts/bootstrap.sh"
cp -a "${REPO_ROOT}/dist/scripts/lib/." "${DEST_DIST}/scripts/lib/"
install -m 0755 "${REPO_ROOT}/dist/bin/evo-rtc-wake" \
    "${DEST_DIST}/bin/evo-rtc-wake"
install -m 0644 "${REPO_ROOT}/dist/sudoers.d/evo-rtc-wake.in" \
    "${DEST_DIST}/sudoers.d/evo-rtc-wake.in"
install -m 0644 "${REPO_ROOT}/dist/systemd/evo.service" \
    "${DEST_DIST}/systemd/evo.service"
