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

# Shared OOP plugin list — MUST stay identical to
# deploy-distribution.sh. Sourced from one file so a reinstall
# bundle cannot silently ship a subset of the deploy path.
# Format: <plugin-name>:<plugin-crate>:<wire-binary-name>:<features>
# shellcheck source=lib/oop-plugins.sh
source "${REPO_ROOT}/dist/scripts/lib/oop-plugins.sh"

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
    # Privileges contract — every bundled plugin MUST ship its
    # privileges.yaml alongside the manifest and binary. The
    # bundle's install-time preflight (`evo-install.sh` +
    # `evo-plugin-tool install`) reads this file for the
    # `has_os_dependencies` flag and the `required_binaries`
    # list, and refuses to promote the bundle if it is absent.
    # Not signed (evo-plugin-tool's `sign` covers manifest +
    # binary only per `signing_message`); this is a runtime-
    # readable declaration whose integrity comes from being
    # inside the signature-covered tarball.
    p_privileges_src="${REPO_ROOT}/plugins/${p_name}/privileges.yaml"
    if [[ ! -f "${p_privileges_src}" ]]; then
        echo "FAIL: ${p_name} privileges.yaml missing at ${p_privileges_src}" >&2
        exit 2
    fi
    install -m 0644 "${p_privileges_src}" "${p_dir}/privileges.yaml"
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
# Placement primitive. evo-install.sh delegates all /etc
# placement (asound.conf substitution, sudoers, drop-ins,
# plugins.d defaults, mpd include, trust roots, avahi disable)
# to bootstrap.sh inside the staged bundle. One canonical
# placement primitive eliminates the parallel-truth-path
# regression class. The lib/ directory carries the sourced
# helper modules bootstrap.sh expects to find (currently
# detect-audio-card.sh).
install -d -m 0755 "${STAGE_DIR}/dist/scripts"
install -m 0755 "${REPO_ROOT}/dist/scripts/bootstrap.sh" \
    "${STAGE_DIR}/dist/scripts/bootstrap.sh"
install -d -m 0755 "${STAGE_DIR}/dist/scripts/lib"
cp -a "${REPO_ROOT}/dist/scripts/lib/." \
    "${STAGE_DIR}/dist/scripts/lib/"
# ALSA + MPD reference configs.
install -d -m 0755 "${STAGE_DIR}/dist/alsa"
cp -a "${REPO_ROOT}/dist/alsa/." "${STAGE_DIR}/dist/alsa/"
install -d -m 0755 "${STAGE_DIR}/dist/mpd"
cp -a "${REPO_ROOT}/dist/mpd/." "${STAGE_DIR}/dist/mpd/"
# Distribution-tier plugin defaults. bootstrap.sh reads from
# `${DIST_DIR}/plugins.d/` to install the network plugin's
# `probe_kind = off` default and to render the multiroom
# template against the operator's --multiroom-* flags. Without
# this in the bundle, both files silently skip — the network
# plugin admits with its code defaults and the multiroom plugin
# does not get configured at install time.
install -d -m 0755 "${STAGE_DIR}/dist/plugins.d"
cp -a "${REPO_ROOT}/dist/plugins.d/." "${STAGE_DIR}/dist/plugins.d/"
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
# Distribution-tier binaries (evo-captive-probe wrapper). bootstrap.sh
# reads these from `$DIST_DIR/bin/*` at install time; without staging
# here, Step 1b2 (captive-probe wrapper install) fails.
if [[ -d "${REPO_ROOT}/dist/bin" ]]; then
    install -d -m 0755 "${STAGE_DIR}/dist/bin"
    cp -a "${REPO_ROOT}/dist/bin/." "${STAGE_DIR}/dist/bin/"
fi
# Plugin-adjacent dist trees. bootstrap.sh references narrow
# wrappers + sudoers templates that live INSIDE each plugin's
# repo directory (each plugin owns its own privilege surface):
#   - network.smb-server: evo-smb-user-sync wrapper + evo-samba-server
#     sudoers template
#   - network.shares: evo-network-shares sudoers template
# The bundle stages each plugin's `dist/` subtree next to its
# binary + manifest so the bootstrap's `$DIST_DIR/../plugins/*/dist/*`
# paths resolve inside the stage.
for plugin_with_dist in \
    "org.evoframework.network.smb-server" \
    "org.evoframework.network.shares" ; do
    p_dist_src="${REPO_ROOT}/plugins/${plugin_with_dist}/dist"
    if [[ -d "${p_dist_src}" ]]; then
        install -d -m 0755 "${STAGE_DIR}/plugins/${plugin_with_dist}/dist"
        cp -a "${p_dist_src}/." "${STAGE_DIR}/plugins/${plugin_with_dist}/dist/"
    fi
done
# README (operator-readable narrative; the installer prints
# the bring-up procedure section as part of its summary).
install -m 0644 "${REPO_ROOT}/dist/README.md" \
    "${STAGE_DIR}/dist/README.md"
echo "  ok"

# --------------------------------------------------------------
# Sibling layers: evo-device-boot (Plymouth splash) + evo-kiosk-eng
# (labwc session + WebKit browser).
# --------------------------------------------------------------
# The bundle is the operator's single source of truth for a
# fresh install. A device that
# receives the bundle and runs `evo-install.sh` must come up
# with the full stack: boot splash, audio distribution, kiosk
# session. Vendor distributions that want a headless variant
# opt out by setting `EVO_INSTALL_BOOT_LAYER=0` and/or
# `EVO_INSTALL_KIOSK_LAYER=0` on the target — see bootstrap.sh
# Step 4.
#
# Layer trees are staged under `${STAGE_DIR}/layers/<name>/` so
# bootstrap.sh reads them at `${DIST_DIR}/../layers/<name>/`.
# Each layer's own installer is idempotent and self-contained
# per its owning repo's contract; no vendorization — the bundle
# just copies the tree byte-for-byte at build time.
#
# Sibling repos are resolved relative to REPO_ROOT (checked-out
# `evo-device-audio`); dev-box layout has all three side-by-side.
echo "[3b/5] stage sibling layers (evo-device-boot + evo-kiosk-eng) ..."
BOOT_ROOT="$(cd "${REPO_ROOT}/../evo-device-boot" 2>/dev/null && pwd)" || {
    echo "FAIL: evo-device-boot repo not found at ${REPO_ROOT}/../evo-device-boot" >&2
    echo "      (the audio distribution's bundle composes the boot theme" >&2
    echo "       layer — check out https://github.com/foonerd/evo-device-boot" >&2
    echo "       adjacent to evo-device-audio, or set EVO_BUNDLE_SKIP_BOOT_LAYER=1)" >&2
    if [[ "${EVO_BUNDLE_SKIP_BOOT_LAYER:-0}" != "1" ]]; then exit 2; fi
    BOOT_ROOT=""
}
KIOSK_ROOT="$(cd "${REPO_ROOT}/../evo-kiosk-eng" 2>/dev/null && pwd)" || {
    echo "FAIL: evo-kiosk-eng repo not found at ${REPO_ROOT}/../evo-kiosk-eng" >&2
    echo "      (the audio distribution's bundle composes the kiosk session" >&2
    echo "       layer — check out foonerd/evo-kiosk-eng adjacent to" >&2
    echo "       evo-device-audio, or set EVO_BUNDLE_SKIP_KIOSK_LAYER=1)" >&2
    if [[ "${EVO_BUNDLE_SKIP_KIOSK_LAYER:-0}" != "1" ]]; then exit 2; fi
    KIOSK_ROOT=""
}

if [[ -n "${BOOT_ROOT}" ]]; then
    install -d -m 0755 "${STAGE_DIR}/layers/evo-device-boot"
    for sub in scripts plymouth fbcon-logo grub-theme systemd tools; do
        if [[ -d "${BOOT_ROOT}/${sub}" ]]; then
            cp -a "${BOOT_ROOT}/${sub}" "${STAGE_DIR}/layers/evo-device-boot/"
        fi
    done
    if [[ -f "${BOOT_ROOT}/DESIGN.md" ]]; then
        install -m 0644 "${BOOT_ROOT}/DESIGN.md" \
            "${STAGE_DIR}/layers/evo-device-boot/DESIGN.md"
    fi
    if [[ -f "${BOOT_ROOT}/README.md" ]]; then
        install -m 0644 "${BOOT_ROOT}/README.md" \
            "${STAGE_DIR}/layers/evo-device-boot/README.md"
    fi
    echo "  ok evo-device-boot (from ${BOOT_ROOT})"
fi

UI_SHELL_ROOT="$(cd "${REPO_ROOT}/../evo-ui-eng/apps/evo-ui-shell" 2>/dev/null && pwd)" || {
    echo "FAIL: evo-ui-eng/apps/evo-ui-shell not found at ${REPO_ROOT}/../evo-ui-eng/apps/evo-ui-shell" >&2
    echo "      (the audio distribution's bundle composes the UI shell — " >&2
    echo "       check out foonerd/evo-ui-eng adjacent to evo-device-audio, or" >&2
    echo "       set EVO_BUNDLE_SKIP_UI_SHELL=1)" >&2
    if [[ "${EVO_BUNDLE_SKIP_UI_SHELL:-0}" != "1" ]]; then exit 2; fi
    UI_SHELL_ROOT=""
}

if [[ -n "${UI_SHELL_ROOT}" ]]; then
    # UI SPA build output goes at bundle root as `ui/` — the
    # installer places it at /opt/evo/ui/, which is what
    # `dist/systemd/evo.service.d/https.conf` pins in
    # `EVO_HTTPS_STATIC_DIR`. Without this, the steward logs
    # "EVO_HTTPS_STATIC_DIR points at a path that does not
    # exist or is not a directory; static-asset serving
    # disabled" and every browser hit to https://<device>:8443/
    # returns nothing but the API surface — the operator UI is
    # unreachable.
    if [[ -d "${UI_SHELL_ROOT}/dist" ]]; then
        install -d -m 0755 "${STAGE_DIR}/ui"
        cp -a "${UI_SHELL_ROOT}/dist/." "${STAGE_DIR}/ui/"
        # The distribution's setup.html overlay lands next to
        # the SPA so the first-run pair page shares an origin
        # with the shell.
        if [[ -f "${REPO_ROOT}/dist/ui-overlay/setup.html" ]]; then
            install -m 0644 "${REPO_ROOT}/dist/ui-overlay/setup.html" \
                "${STAGE_DIR}/ui/setup.html"
        fi
        echo "  ok evo-ui-shell (from ${UI_SHELL_ROOT}/dist)"
    else
        echo "FAIL: evo-ui-shell has no built dist/ tree at ${UI_SHELL_ROOT}/dist" >&2
        echo "      Run \`bun run build\` (or the UI team's equivalent) inside" >&2
        echo "      ${UI_SHELL_ROOT} before building the audio bundle." >&2
        exit 2
    fi

    # evo-ui-runtime — the operator-facing HTTP/HTTPS listener
    # on ports 80/443 that serves the SPA and reverse-proxies
    # the framework's :8443 wire surface at the same origin.
    # Distinct from the framework's own HTTPS listener (which
    # only speaks the wire, does not serve static assets in a
    # release-layout shape). The runtime binary is what the
    # `evo-ui.service` systemd unit executes; without it the
    # unit fail-loops with `status=209/STDOUT` and the operator
    # UI is unreachable on the default ports.
    #
    # Prebuilt binaries live at
    # `apps/evo-ui-runtime/target/<triple>/release/evo-ui-runtime`
    # under evo-ui-eng. Match the current TARGET_TRIPLE; the
    # UI team builds x86_64 + aarch64 prebuilts alongside every
    # release.
    UI_RUNTIME_BIN="${REPO_ROOT}/../evo-ui-eng/apps/evo-ui-runtime/target/${TARGET_TRIPLE}/release/evo-ui-runtime"
    UI_SERVICE_TEMPLATE="${REPO_ROOT}/../evo-ui-eng/apps/evo-ui-runtime/scripts/device/evo-ui.service.in"
    if [[ -x "${UI_RUNTIME_BIN}" && -f "${UI_SERVICE_TEMPLATE}" ]]; then
        install -d -m 0755 "${STAGE_DIR}/ui-runtime"
        install -m 0755 "${UI_RUNTIME_BIN}" \
            "${STAGE_DIR}/ui-runtime/evo-ui-runtime"
        install -m 0644 "${UI_SERVICE_TEMPLATE}" \
            "${STAGE_DIR}/ui-runtime/evo-ui.service.in"
        echo "  ok evo-ui-runtime ($(basename "${UI_RUNTIME_BIN}") + evo-ui.service.in for ${TARGET_TRIPLE})"
    else
        echo "FAIL: evo-ui-runtime prebuilt missing for ${TARGET_TRIPLE}" >&2
        echo "      Expected binary: ${UI_RUNTIME_BIN}" >&2
        echo "      Expected unit template: ${UI_SERVICE_TEMPLATE}" >&2
        echo "      Build with \`cargo build --release --target ${TARGET_TRIPLE}\` inside" >&2
        echo "      \$(dirname \"${UI_RUNTIME_BIN}\")/../../.." >&2
        exit 2
    fi
fi

if [[ -n "${KIOSK_ROOT}" ]]; then
    install -d -m 0755 "${STAGE_DIR}/layers/evo-kiosk-eng"
    # scripts/ + layer/ carry the installer + the runtime
    # assets (labwc config, systemd unit, kiosk.privileges.toml,
    # trust root, helper binaries, prebuilt browser binaries
    # per triple).
    for sub in scripts layer; do
        if [[ -d "${KIOSK_ROOT}/${sub}" ]]; then
            cp -a "${KIOSK_ROOT}/${sub}" "${STAGE_DIR}/layers/evo-kiosk-eng/"
        fi
    done
    # crates/ carries the kiosk-browser source tree; bundled so
    # `cargo` mode works on target triples with no prebuilt.
    # Excludes target/ (build artefacts) via cp -a on the src
    # subtree only. Cargo.toml + Cargo.lock at repo root are
    # required to resolve the workspace member.
    if [[ -d "${KIOSK_ROOT}/crates" ]]; then
        cp -a "${KIOSK_ROOT}/crates" "${STAGE_DIR}/layers/evo-kiosk-eng/"
    fi
    for f in Cargo.toml Cargo.lock README.md DEVELOPING.md; do
        if [[ -f "${KIOSK_ROOT}/${f}" ]]; then
            install -m 0644 "${KIOSK_ROOT}/${f}" \
                "${STAGE_DIR}/layers/evo-kiosk-eng/${f}"
        fi
    done
    # Cross.toml is dev-tooling; bundle it so anyone rebuilding
    # inside the bundle stage has the same cross-arch shape.
    if [[ -f "${KIOSK_ROOT}/Cross.toml" ]]; then
        install -m 0644 "${KIOSK_ROOT}/Cross.toml" \
            "${STAGE_DIR}/layers/evo-kiosk-eng/Cross.toml"
    fi
    echo "  ok evo-kiosk-eng (from ${KIOSK_ROOT})"
fi

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
