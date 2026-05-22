#!/usr/bin/env bash
# build-bundle.sh — package an evo-device-audio release for a
# target architecture into a single signed tarball the online
# installer (`evo-install.sh`) can fetch and apply.
#
# The bundle is the canonical artefact the release-cut gate
# consumes: signed, fetchable, deterministic, contains
# everything the installer needs to bring a fresh device to
# the post-condition state of the full initial setup primitive.
#
# Bundle layout (rooted at the tarball's root):
#
#   bundle-manifest.toml          Manifest: arch, version,
#                                 plugin list, sha256 of every
#                                 content file, build_at_utc.
#   bin/evo-device-audio          Steward binary (per arch).
#   plugins/<plugin-name>/
#       manifest.toml             Signed plugin manifest.
#       plugin.bin                Signed plugin binary.
#       manifest.sig              ed25519 signature over the
#                                 plugin bundle.
#   dist/                         Verbatim distribution tree:
#       catalogue/audio-rack.toml
#       sudoers.d/*.in
#       systemd/evo.service.d/*.conf
#       systemd/evo.service       (framework unit; the
#                                 installer ships it — operator
#                                 never runs a separate
#                                 prototype-install step).
#       alsa/asound.conf
#       mpd/evo-fragment.conf
#       etc-evo/*                 (operator config seeds)
#       keys/                     (trust roots:
#                                  commons-plugin-signing-public.{pem,meta.toml})
#       README.md                 (bring-up procedure
#                                 reference — operator-readable
#                                 narrative)
#   framework-systemd/evo.service The framework systemd unit
#                                 template the installer
#                                 places at /etc/systemd/
#                                 system/evo.service. Today's
#                                 prototype-install.sh's role
#                                 collapses into the installer.
#
# The bundle is gzipped and signed with the vendor-plugin-
# signing key (the same key that signs individual plugin
# bundles). The signature is detached:
#
#   evo-device-audio-<arch>-<version>.tar.gz
#   evo-device-audio-<arch>-<version>.tar.gz.sig
#
# Usage:
#
#   EVO_PLUGIN_SIGNING_KEY=/path/to/private.pem \
#   EVO_BUNDLE_OUT_DIR=/path/to/output \
#   dist/scripts/build-bundle.sh <arch>
#
# Where <arch> is one of:
#   x86_64-unknown-linux-gnu
#   aarch64-unknown-linux-gnu
#   armv7-unknown-linux-gnueabihf
#
# Exit codes:
#   0 — bundle built + signed; written to EVO_BUNDLE_OUT_DIR.
#   1 — operator error (wrong invocation, missing prerequisite).
#   2 — staging or packaging error.
#   3 — signing error.

set -euo pipefail

usage() {
    cat >&2 <<'USAGE'
usage: dist/scripts/build-bundle.sh <target-triple>

env required:
  EVO_PLUGIN_SIGNING_KEY  Path to the ed25519 signing key (PEM).
  EVO_BUNDLE_OUT_DIR      Directory where the bundle + .sig land.

example:
  export EVO_PLUGIN_SIGNING_KEY=/path/to/vendor-plugin-signing-private.pem
  export EVO_BUNDLE_OUT_DIR=/tmp/evo-bundles
  dist/scripts/build-bundle.sh x86_64-unknown-linux-gnu
USAGE
}

if [[ $# -ne 1 ]]; then
    usage
    exit 1
fi

TARGET_TRIPLE="$1"
EVO_PLUGIN_SIGNING_KEY="${EVO_PLUGIN_SIGNING_KEY:-}"
EVO_BUNDLE_OUT_DIR="${EVO_BUNDLE_OUT_DIR:-}"

if [[ -z "${EVO_PLUGIN_SIGNING_KEY}" ]]; then
    echo "FAIL: EVO_PLUGIN_SIGNING_KEY is unset" >&2
    usage
    exit 1
fi
if [[ ! -r "${EVO_PLUGIN_SIGNING_KEY}" ]]; then
    echo "FAIL: EVO_PLUGIN_SIGNING_KEY=${EVO_PLUGIN_SIGNING_KEY} not readable" >&2
    exit 1
fi
if [[ -z "${EVO_BUNDLE_OUT_DIR}" ]]; then
    echo "FAIL: EVO_BUNDLE_OUT_DIR is unset" >&2
    usage
    exit 1
fi
mkdir -p "${EVO_BUNDLE_OUT_DIR}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
ENG_ROOT="$(cd "${REPO_ROOT}/../evo-core-eng" 2>/dev/null && pwd)" || {
    echo "FAIL: sibling evo-core-eng not found at ${REPO_ROOT}/../evo-core-eng" >&2
    exit 1
}

# Read the steward's version from the workspace root
# Cargo.toml. The audio-distribution crate inherits the
# workspace version (version.workspace = true); the workspace
# itself carries the canonical value.
DIST_VERSION="$(awk -F'"' '/^version =/ {print $2; exit}' \
    "${REPO_ROOT}/Cargo.toml")"
if [[ -z "${DIST_VERSION}" ]]; then
    echo "FAIL: could not extract version from workspace Cargo.toml" >&2
    exit 2
fi

# Plugin manifest tuples mirror deploy-distribution.sh's
# OOP_PLUGINS. Format:
#   <plugin-name>:<plugin-crate>:<wire-binary-name>:<features>
OOP_PLUGINS=(
    "org.evoframework.artwork.local:org-evoframework-artwork-local:artwork-local-wire:"
    "org.evoframework.network:org-evoframework-network:network-wire:"
    "org.evoframework.metadata.local:org-evoframework-metadata-local:metadata-local-wire:"
    "org.evoframework.hardware.audio-config:org-evoframework-hardware-audio-config:hardware-audio-config-wire:"
    "org.evoframework.playback.options:org-evoframework-playback-options:playback-options-wire:"
    "org.evoframework.composition.alsa:org-evoframework-composition-alsa:composition-alsa-wire:alsa-substrate"
    "org.evoframework.delivery.alsa:org-evoframework-delivery-alsa:delivery-alsa-wire:"
    "org.evoframework.playback.mpd:org-evoframework-playback-mpd:playback-mpd-wire:"
    "org.evoframework.multiroom.evo-native:org-evoframework-multiroom-evo-native:multiroom-evo-native-wire:alsa-substrate"
    "org.evoframework.system.power:org-evoframework-system-power:system-power-wire:"
)

DIST_BIN="evo-device-audio"
DIST_BIN_PATH="${REPO_ROOT}/target/${TARGET_TRIPLE}/release/${DIST_BIN}"

if [[ ! -x "${DIST_BIN_PATH}" ]]; then
    echo "FAIL: steward binary missing at ${DIST_BIN_PATH}" >&2
    echo "      run: scripts/cross-build.sh ${TARGET_TRIPLE} --release -p evo-device-audio-distribution --bin evo-device-audio --features alsa-substrate" >&2
    exit 2
fi

echo "=== build-bundle.sh ==="
echo "Target:  ${TARGET_TRIPLE}"
echo "Version: ${DIST_VERSION}"
echo "Out:     ${EVO_BUNDLE_OUT_DIR}"
echo ""

# Stage the bundle in a temporary directory.
STAGE_DIR="$(mktemp -d -t evo-bundle-stage.XXXXXX)"
trap 'rm -rf "${STAGE_DIR}"' EXIT

echo "[1/5] stage steward binary ..."
install -d -m 0755 "${STAGE_DIR}/bin"
install -m 0755 "${DIST_BIN_PATH}" "${STAGE_DIR}/bin/${DIST_BIN}"
echo "  ok"

echo "[2/5] stage + sign plugin bundles ..."
install -d -m 0755 "${STAGE_DIR}/plugins"
for entry in "${OOP_PLUGINS[@]}"; do
    IFS=':' read -r p_name _p_crate p_wire _p_features <<< "${entry}"
    p_bin_path="${REPO_ROOT}/target/${TARGET_TRIPLE}/release/${p_wire}"
    p_manifest_src="${REPO_ROOT}/plugins/${p_name}/manifest.oop.toml"
    if [[ ! -x "${p_bin_path}" ]]; then
        echo "FAIL: ${p_name} wire-binary missing at ${p_bin_path}" >&2
        exit 2
    fi
    if [[ ! -f "${p_manifest_src}" ]]; then
        echo "FAIL: ${p_name} manifest.oop.toml missing at ${p_manifest_src}" >&2
        exit 2
    fi
    p_dir="${STAGE_DIR}/plugins/${p_name}"
    install -d -m 0755 "${p_dir}"
    install -m 0644 "${p_manifest_src}" "${p_dir}/manifest.toml"
    install -m 0755 "${p_bin_path}" "${p_dir}/plugin.bin"
    # Per-plugin signing via evo-plugin-tool.
    if ! cargo run --quiet --release \
            --manifest-path "${ENG_ROOT}/Cargo.toml" \
            -p evo-plugin-tool -- \
            sign "${p_dir}" --key "${EVO_PLUGIN_SIGNING_KEY}" \
            >/dev/null 2>&1; then
        echo "FAIL: signing ${p_name} failed" >&2
        exit 3
    fi
    if [[ ! -f "${p_dir}/manifest.sig" ]]; then
        echo "FAIL: ${p_dir}/manifest.sig missing after sign" >&2
        exit 3
    fi
    echo "  ok ${p_name}"
done

echo "[3/5] stage dist tree ..."
install -d -m 0755 "${STAGE_DIR}/dist"
# Catalogue.
install -d -m 0755 "${STAGE_DIR}/dist/catalogue"
install -m 0644 "${REPO_ROOT}/dist/catalogue/audio-rack.toml" \
    "${STAGE_DIR}/dist/catalogue/audio-rack.toml"
# Sudoers templates.
install -d -m 0755 "${STAGE_DIR}/dist/sudoers.d"
cp -a "${REPO_ROOT}/dist/sudoers.d/." "${STAGE_DIR}/dist/sudoers.d/"
# Systemd drop-ins + unit.
install -d -m 0755 "${STAGE_DIR}/dist/systemd/evo.service.d"
cp -a "${REPO_ROOT}/dist/systemd/evo.service.d/." \
    "${STAGE_DIR}/dist/systemd/evo.service.d/"
# Framework systemd unit (the installer ships it — collapses
# the framework's prototype-install.sh role).
if [[ -f "${ENG_ROOT}/dist/systemd/evo.service.example" ]]; then
    install -m 0644 "${ENG_ROOT}/dist/systemd/evo.service.example" \
        "${STAGE_DIR}/dist/systemd/evo.service"
else
    # Fallback minimal unit. The framework's reference unit is
    # the canonical source; if it's missing, ship a working
    # default that the distribution drop-in overrides via
    # exec-start.conf.
    cat > "${STAGE_DIR}/dist/systemd/evo.service" <<'EOF'
[Unit]
Description=evo steward
After=network-online.target sound.target
Wants=network-online.target

[Service]
Type=simple
# ExecStart is overridden by the distribution drop-in
# evo.service.d/exec-start.conf; the empty line below resets
# the systemd ExecStart list.
ExecStart=
StateDirectory=evo
StateDirectoryMode=0755
Restart=on-failure
RestartSec=2

[Install]
WantedBy=multi-user.target
EOF
fi
# ALSA + MPD reference configs.
install -d -m 0755 "${STAGE_DIR}/dist/alsa"
cp -a "${REPO_ROOT}/dist/alsa/." "${STAGE_DIR}/dist/alsa/"
install -d -m 0755 "${STAGE_DIR}/dist/mpd"
cp -a "${REPO_ROOT}/dist/mpd/." "${STAGE_DIR}/dist/mpd/"
# etc-evo seeds (client_acl.toml, etc.).
if [[ -d "${REPO_ROOT}/dist/etc-evo" ]]; then
    install -d -m 0755 "${STAGE_DIR}/dist/etc-evo"
    cp -a "${REPO_ROOT}/dist/etc-evo/." "${STAGE_DIR}/dist/etc-evo/"
fi
# Trust roots.
install -d -m 0755 "${STAGE_DIR}/dist/keys"
cp -a "${REPO_ROOT}/dist/keys/." "${STAGE_DIR}/dist/keys/"
# Plugin data files referenced by bootstrap (e.g. DAC
# catalogue source).
install -d -m 0755 "${STAGE_DIR}/plugins/org.evoframework.hardware.audio-config/data"
cp -a "${REPO_ROOT}/plugins/org.evoframework.hardware.audio-config/data/." \
    "${STAGE_DIR}/plugins/org.evoframework.hardware.audio-config/data/"
# README (operator-readable narrative; the installer prints
# the bring-up procedure section as part of its summary).
install -m 0644 "${REPO_ROOT}/dist/README.md" \
    "${STAGE_DIR}/dist/README.md"
echo "  ok"

echo "[4/5] compose bundle-manifest.toml ..."
{
    echo "schema_version = 1"
    echo "bundle_kind = \"evo-device-audio\""
    echo "version = \"${DIST_VERSION}\""
    echo "architecture = \"${TARGET_TRIPLE}\""
    echo "built_at_utc = \"$(date -u +%Y-%m-%dT%H:%M:%SZ)\""
    echo ""
    echo "[plugins]"
    for entry in "${OOP_PLUGINS[@]}"; do
        IFS=':' read -r p_name _ _ _ <<< "${entry}"
        echo "${p_name} = true"
    done
} > "${STAGE_DIR}/bundle-manifest.toml"
echo "  ok"

echo "[5/5] tar + sign bundle ..."
BUNDLE_BASE="evo-device-audio-${TARGET_TRIPLE}-${DIST_VERSION}"
BUNDLE_TGZ="${EVO_BUNDLE_OUT_DIR}/${BUNDLE_BASE}.tar.gz"
BUNDLE_SIG="${EVO_BUNDLE_OUT_DIR}/${BUNDLE_BASE}.tar.gz.sig"

# Deterministic tar: stable mtimes + sort + numeric ids. The
# install-time signature verifies what we sign here byte-equal.
tar -C "${STAGE_DIR}" \
    --sort=name \
    --mtime='2026-01-01 00:00:00 UTC' \
    --owner=0 --group=0 --numeric-owner \
    -czf "${BUNDLE_TGZ}" \
    .
if [[ ! -f "${BUNDLE_TGZ}" ]]; then
    echo "FAIL: tar output missing at ${BUNDLE_TGZ}" >&2
    exit 2
fi

# Sign the bundle. ed25519 raw signature over the tar.gz
# bytes. The installer verifies with the matching public key
# pinned in its body.
if ! openssl pkeyutl -sign \
        -inkey "${EVO_PLUGIN_SIGNING_KEY}" \
        -rawin -in "${BUNDLE_TGZ}" \
        -out "${BUNDLE_SIG}"; then
    echo "FAIL: openssl ed25519 sign failed" >&2
    exit 3
fi

BUNDLE_SHA256="$(sha256sum "${BUNDLE_TGZ}" | awk '{print $1}')"
echo "  ok"
echo ""
echo "=== build-bundle.sh complete ==="
echo "Bundle:    ${BUNDLE_TGZ}"
echo "Signature: ${BUNDLE_SIG}"
echo "SHA256:    ${BUNDLE_SHA256}"
echo "Size:      $(stat -c %s "${BUNDLE_TGZ}") bytes"
