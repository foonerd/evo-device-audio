#!/usr/bin/env bash
# stage-audio-dist-tree.sh — the dist/ placement tree that first-boot
# composition copies into the installer tarball. Also used as the
# evo-device-audio-dist piece payload.
#
# Usage: stage-audio-dist-tree.sh --repo-root PATH --out-dir PATH

set -euo pipefail

REPO_ROOT=""
OUT=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo-root) REPO_ROOT="$2"; shift 2 ;;
        --out-dir)   OUT="$2"; shift 2 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

[[ -n "${REPO_ROOT}" && -n "${OUT}" ]] || { echo "usage: stage-audio-dist-tree.sh --repo-root PATH --out-dir PATH" >&2; exit 2; }
[[ -d "${REPO_ROOT}/dist" ]] || { echo "no dist/ under ${REPO_ROOT}" >&2; exit 2; }

mkdir -p "${OUT}/dist/catalogue" "${OUT}/dist/sudoers.d" \
    "${OUT}/dist/systemd/evo.service.d" "${OUT}/dist/scripts/lib" \
    "${OUT}/dist/alsa" "${OUT}/dist/mpd" "${OUT}/dist/plugins.d" \
    "${OUT}/dist/keys"

install -m 0644 "${REPO_ROOT}/dist/catalogue/audio-rack.toml" \
    "${OUT}/dist/catalogue/audio-rack.toml"
cp -a "${REPO_ROOT}/dist/sudoers.d/." "${OUT}/dist/sudoers.d/"
cp -a "${REPO_ROOT}/dist/systemd/evo.service.d/." "${OUT}/dist/systemd/evo.service.d/"
if [[ ! -f "${REPO_ROOT}/dist/systemd/evo.service" ]]; then
    echo "stage-audio-dist-tree: missing dist/systemd/evo.service" >&2
    exit 1
fi
install -m 0644 "${REPO_ROOT}/dist/systemd/evo.service"     "${OUT}/dist/systemd/evo.service"
install -m 0755 "${REPO_ROOT}/dist/scripts/bootstrap.sh" "${OUT}/dist/scripts/bootstrap.sh"
cp -a "${REPO_ROOT}/dist/scripts/lib/." "${OUT}/dist/scripts/lib/"
cp -a "${REPO_ROOT}/dist/alsa/." "${OUT}/dist/alsa/"
cp -a "${REPO_ROOT}/dist/mpd/." "${OUT}/dist/mpd/"
cp -a "${REPO_ROOT}/dist/plugins.d/." "${OUT}/dist/plugins.d/"
cp -a "${REPO_ROOT}/dist/keys/." "${OUT}/dist/keys/"
install -m 0644 "${REPO_ROOT}/dist/README.md" "${OUT}/dist/README.md"

if [[ -d "${REPO_ROOT}/dist/etc-evo" ]]; then
    mkdir -p "${OUT}/dist/etc-evo"
    cp -a "${REPO_ROOT}/dist/etc-evo/." "${OUT}/dist/etc-evo/"
fi
if [[ -d "${REPO_ROOT}/dist/bin" ]]; then
    mkdir -p "${OUT}/dist/bin"
    cp -a "${REPO_ROOT}/dist/bin/." "${OUT}/dist/bin/"
fi
if [[ -f "${REPO_ROOT}/dist/ui-overlay/setup.html" ]]; then
    mkdir -p "${OUT}/dist/ui-overlay"
    install -m 0644 "${REPO_ROOT}/dist/ui-overlay/setup.html" \
        "${OUT}/dist/ui-overlay/setup.html"
fi

# Plugin-adjacent trees bootstrap reads from the staged plugin dirs.
if [[ -d "${REPO_ROOT}/plugins/org.evoframework.hardware.audio-config/data" ]]; then
    mkdir -p "${OUT}/plugin-overlays/org.evoframework.hardware.audio-config/data"
    cp -a "${REPO_ROOT}/plugins/org.evoframework.hardware.audio-config/data/." \
        "${OUT}/plugin-overlays/org.evoframework.hardware.audio-config/data/"
fi
for plugin_with_dist in \
    org.evoframework.network.smb-server \
    org.evoframework.network.shares \
    org.evoframework.storage.usb; do
    if [[ -d "${REPO_ROOT}/plugins/${plugin_with_dist}/dist" ]]; then
        mkdir -p "${OUT}/plugin-overlays/${plugin_with_dist}/dist"
        cp -a "${REPO_ROOT}/plugins/${plugin_with_dist}/dist/." \
            "${OUT}/plugin-overlays/${plugin_with_dist}/dist/"
    fi
done
