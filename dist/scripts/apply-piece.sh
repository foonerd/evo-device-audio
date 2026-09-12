#!/usr/bin/env bash
# apply-piece.sh — overlay one published piece onto a live device.
# First piece: evo-ui-shell. Replaces /opt/evo/ui release + flat
# copy (framework :8443) and restarts evo-ui if present.
#
# Usage (root):
#   evo-install.sh --piece evo-ui-shell --version 0.1.13
#   apply-piece.sh --piece evo-ui-shell --version 0.1.13

set -euo pipefail

PIECE="${EVO_PIECE:-}"
VERSION="${EVO_PIECE_VERSION:-}"
ARTEFACTS_RAW="${EVO_ARTEFACTS_RAW:-https://raw.githubusercontent.com/foonerd/evo-device-audio-artefacts/main}"
SERVICE_USER="${EVO_SERVICE_USER:-}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --piece)   PIECE="$2"; shift 2 ;;
        --version) VERSION="$2"; shift 2 ;;
        *) echo "FAIL: unknown argument: $1" >&2; exit 1 ;;
    esac
done

[[ -n "${PIECE}" && -n "${VERSION}" ]] || { echo "FAIL: --piece and --version required" >&2; exit 1; }
[[ "$(id -u)" -eq 0 ]] || { echo "FAIL: must run as root" >&2; exit 1; }

if [[ -z "${SERVICE_USER}" && -n "${SUDO_USER:-}" && "${SUDO_USER}" != "root" ]]; then
    SERVICE_USER="${SUDO_USER}"
fi
if [[ -z "${SERVICE_USER}" ]]; then
    SERVICE_USER="$(awk -F: '$3 >= 1000 && $3 < 65000 {print $1; exit}' /etc/passwd)"
fi

# Same trust root as evo-install.sh (commons public key).
TRUST_ROOT="${EVO_BUNDLE_TRUST_ROOT_PEM:-}"
if [[ -z "${TRUST_ROOT}" ]]; then
    TRUST_ROOT="-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAvJqIhluihUhLY435rJZnIjskDS9affTKSDUIYVIjVE0=
-----END PUBLIC KEY-----"
fi

fetch_verify_tree() {
    local rel="$1"
    local tmp="$2"
    mkdir -p "${tmp}"
    curl -fsSL --connect-timeout 10 --max-time 300 \
        -o "${tmp}/tree.tar.gz" "${ARTEFACTS_RAW}/${rel}/tree.tar.gz"
    curl -fsSL --connect-timeout 10 --max-time 60 \
        -o "${tmp}/tree.tar.gz.sig" "${ARTEFACTS_RAW}/${rel}/tree.tar.gz.sig"
    local key
    key="$(mktemp)"
    printf '%s\n' "${TRUST_ROOT}" > "${key}"
    if ! openssl pkeyutl -verify -pubin -inkey "${key}" -rawin \
            -in "${tmp}/tree.tar.gz" -sigfile "${tmp}/tree.tar.gz.sig"; then
        rm -f "${key}"
        echo "FAIL: signature verify failed for ${rel}" >&2
        exit 3
    fi
    rm -f "${key}"
    tar -xzf "${tmp}/tree.tar.gz" -C "${tmp}"
}

apply_ui_shell() {
    local tmp
    tmp="$(mktemp -d -t evo-piece-XXXXXX)"
    fetch_verify_tree "bundles/evo-ui-shell/${VERSION}" "${tmp}"
    local src
    src="$(find "${tmp}" -mindepth 1 -maxdepth 1 -type d | head -1)"
    [[ -d "${src}" ]] || { echo "FAIL: unpacked ui-shell tree missing" >&2; exit 4; }

    install -d -m 0755 -o root -g root /opt/evo/ui
    install -d -m 0755 -o "${SERVICE_USER}" -g "${SERVICE_USER}" \
        /opt/evo/ui/releases /opt/evo/ui/data /opt/evo/ui/logs
    local release_id release_dir
    release_id="$(date -u +%Y%m%dT%H%M%SZ)"
    release_dir="/opt/evo/ui/releases/${release_id}"
    install -d -m 0755 -o "${SERVICE_USER}" -g "${SERVICE_USER}" "${release_dir}"
    # Preserve first-run setup.html if the piece did not ship one.
    local keep_setup=""
    if [[ ! -f "${src}/setup.html" && -f /opt/evo/ui/setup.html ]]; then
        keep_setup="$(mktemp)"
        cp -a /opt/evo/ui/setup.html "${keep_setup}"
    fi
    cp -a "${src}/." "${release_dir}/"
    ln -sfn "${release_dir}" /opt/evo/ui/current
    cp -a "${src}/." /opt/evo/ui/
    if [[ -n "${keep_setup}" ]]; then
        install -m 0644 "${keep_setup}" /opt/evo/ui/setup.html
        install -m 0644 "${keep_setup}" "${release_dir}/setup.html"
        rm -f "${keep_setup}"
    fi
    chown -R "${SERVICE_USER}:${SERVICE_USER}" /opt/evo/ui
    rm -rf "${tmp}"
    if systemctl list-unit-files evo-ui.service >/dev/null 2>&1; then
        systemctl restart evo-ui.service || true
    fi
    echo "applied evo-ui-shell ${VERSION} -> /opt/evo/ui"
}

case "${PIECE}" in
    evo-ui-shell) apply_ui_shell ;;
    *)
        echo "FAIL: overlay for '${PIECE}' is not implemented (start with evo-ui-shell)" >&2
        exit 1
        ;;
esac
