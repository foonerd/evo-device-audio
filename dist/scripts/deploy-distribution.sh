#!/usr/bin/env bash
# deploy-distribution.sh — cross-build the evo-device-audio
# steward for a target architecture, ship it to the target host,
# restart the unit, and verify the boot trace.
#
# Idempotent in shape: re-running re-builds + re-ships + restarts
# cleanly. The previous binary is preserved as
# `evo-device-audio.prev` for single-step rollback after deploy.
#
# Prerequisites on the target host:
#   - `prototype-install.sh` (framework-tier base install) has
#     run; `/etc/evo/`, `/opt/evo/`, the trust keys + the
#     framework's systemd unit are in place.
#   - `bootstrap.sh` (distribution-tier install) has run;
#     `/etc/sudoers.d/evo-*`, `/etc/systemd/system/evo.service.d/`
#     drop-ins (including `exec-start.conf` that overrides the
#     framework `ExecStart` to point at `evo-device-audio`), and
#     `/etc/evo/plugins.d/` configs are in place.
#
# Prerequisites on the dev box:
#   - `cross` (https://github.com/cross-rs/cross) installed
#     OR the host's stable toolchain + matching cross-link
#     blocks in `.cargo/config.toml` (the rig's existing setup).
#   - SSH access to the target as the operator-configured
#     service user.
#
# Usage:
#
#   scripts/deploy-distribution.sh <TARGET_HOST> <TARGET_USER> <TARGET_TRIPLE>
#
#   All three arguments are required; the script does not bake
#   in defaults to avoid accidentally deploying to a previously-
#   used host.
#
#   TARGET_HOST   — IP or hostname of the target reachable via
#                   ssh from the dev box.
#   TARGET_USER   — operator-configured service user on the
#                   target (matches the user `bootstrap.sh`
#                   resolved at install time).
#   TARGET_TRIPLE — Rust target triple; the compiled binary's
#                   architecture must match the target's CPU
#                   family. Common values:
#                     aarch64-unknown-linux-gnu  (64-bit ARM)
#                     x86_64-unknown-linux-gnu   (64-bit x86)
#                     armv7-unknown-linux-gnueabihf  (32-bit ARM)
#
# Exit codes:
#   0 — build + deploy + restart succeeded; service active.
#   1 — operator error (wrong invocation, ssh refused, missing
#       prerequisite on target).
#   2 — build error (cross build failed; previous deploy untouched).
#   3 — deploy error (scp / install failed; previous binary remains).
#   4 — verify error (service did not become active within budget).

set -euo pipefail

if [[ $# -lt 3 ]]; then
    echo "usage: $0 <target-host> <target-user> <target-triple>" >&2
    echo "example: $0 host.lan <service-user> aarch64-unknown-linux-gnu" >&2
    exit 1
fi

TARGET_HOST="$1"
TARGET_USER="$2"
TARGET_TRIPLE="$3"
SSH_TARGET="${TARGET_USER}@${TARGET_HOST}"

# Required when OOP_PLUGINS is non-empty. Path to the PKCS#8
# ed25519 private key matching a `*.pem` public key already
# installed under `/etc/evo/trust.d/` on every target. The
# script signs each staged bundle so the steward's discovery
# admits it at the trust class the manifest declares; an
# unsigned bundle is refused at admission with an explicit
# error from the framework, so the only way to reach the
# steady-state lifecycle property is to sign.
#
# Set this in the deploying operator's environment:
#   export EVO_PLUGIN_SIGNING_KEY=/path/to/key.pem
EVO_PLUGIN_SIGNING_KEY="${EVO_PLUGIN_SIGNING_KEY:-}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

# Sibling framework workspace clone hosts the `evo-plugin-tool`
# crate. The signing step shells into it via `--manifest-path`.
ENG_ROOT="$(cd "${REPO_ROOT}/../evo-core-eng" 2>/dev/null && pwd)" || {
    echo "WARN: sibling framework workspace clone not found at ${REPO_ROOT}/../evo-core-eng" >&2
    ENG_ROOT=""
}

# The distribution binary's canonical install path on the target.
TARGET_BIN_PATH="/opt/evo/bin/evo-device-audio"
TARGET_BIN_PREV="/opt/evo/bin/evo-device-audio.prev"

# The crate that produces the bundled steward binary. Same name
# as the binary it builds.
DIST_CRATE="evo-device-audio-distribution"
DIST_BIN="evo-device-audio"

# Out-of-process plugin bundles shipped alongside the steward
# binary. Each entry binds:
#   <plugin-name>:<plugin-crate>:<wire-binary>:<features>
# where <plugin-name> is the canonical name landing under
# `/opt/evo/plugins/<plugin-name>/` on the target,
# <plugin-crate> is the cargo `-p` package id,
# <wire-binary> is the binary cargo emits under
# `target/<triple>/release/`, and <features> is the optional
# `--features` arg the wire-binary cross-build receives
# (empty when no features apply).
#
# Each plugin's `manifest.oop.toml` is renamed to
# `manifest.toml` and the wire binary is renamed to
# `plugin.bin` (matching the manifest's `transport.exec`) when
# the bundle is staged for shipment.
#
# Plugins listed here are NO LONGER admitted via the
# distribution's Phase 1 compile-link path — Phase 2
# discovery picks them up from the search root the framework
# walks at boot, and the install / remove / update lifecycle
# reaches per-plugin without touching the steward binary.
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

# Every reference-distribution plugin now ships out-of-process.
# The framework's audio-routing, multi-room substrate, and
# audio-plane wire-proxies carry every per-plugin handle the
# plugin's `load()` requires across the subprocess boundary.
# Phase 1 compile-link admission is retired for the reference
# plugin set; vendor distributions retain the option of
# compile-link admission for plugins that benefit from in-
# process latency (none of the reference plugins do today).

echo "=== deploy-distribution.sh ==="
echo "Target:        ${SSH_TARGET}"
echo "Target triple: ${TARGET_TRIPLE}"
echo "Repo root:     ${REPO_ROOT}"
echo

# ----------------------------------------------------------
# [0/5] Pre-flight: target reachable + base install present
# + signing key resolvable when OOP plugins are shipped.
# ----------------------------------------------------------
echo "[0/5] pre-flight ..."

if [[ ${#OOP_PLUGINS[@]} -gt 0 ]]; then
    # Verify the commons-plugin trust root is on the target.
    # The framework's discovery refuses an unsigned-or-
    # signed-but-unauthorised bundle with an explicit error;
    # this check fails the deploy upfront with a clear
    # remediation hint instead of leaving the operator to read
    # the journal post-restart.
    if ! ssh -o BatchMode=yes -o ConnectTimeout=5 "${SSH_TARGET}" \
            "test -f /etc/evo/trust.d/commons-plugin-signing-public.pem \
                && test -f /etc/evo/trust.d/commons-plugin-signing-public.meta.toml" \
            >/dev/null 2>&1; then
        echo "FAIL: commons-plugin trust root missing on target" >&2
        echo "      Expected: /etc/evo/trust.d/commons-plugin-signing-public.{pem,meta.toml}" >&2
        echo "      Remediation: (re-)run bootstrap.sh on the target — Step 2.8" >&2
        echo "      installs the distribution-tier plugin trust root." >&2
        exit 1
    fi
fi

if [[ ${#OOP_PLUGINS[@]} -gt 0 ]]; then
    if [[ -z "${EVO_PLUGIN_SIGNING_KEY}" ]]; then
        echo "FAIL: OOP_PLUGINS is non-empty but EVO_PLUGIN_SIGNING_KEY is unset" >&2
        echo "      The steward refuses unsigned bundles at admission. Set:" >&2
        echo "        export EVO_PLUGIN_SIGNING_KEY=/path/to/private-key.pem" >&2
        echo "      The matching public key must already be installed under" >&2
        echo "      /etc/evo/trust.d/ on every target." >&2
        exit 1
    fi
    if [[ ! -r "${EVO_PLUGIN_SIGNING_KEY}" ]]; then
        echo "FAIL: EVO_PLUGIN_SIGNING_KEY=${EVO_PLUGIN_SIGNING_KEY} is not readable" >&2
        exit 1
    fi
    if [[ -z "${ENG_ROOT}" ]]; then
        echo "FAIL: cannot sign bundles — sibling framework workspace clone missing" >&2
        echo "      expected at ${REPO_ROOT}/../evo-core-eng" >&2
        exit 1
    fi
    echo "  ok (signing key: ${EVO_PLUGIN_SIGNING_KEY})"
    echo "  ok (framework tool source: ${ENG_ROOT})"
fi

if ! ssh -o BatchMode=yes -o ConnectTimeout=5 "${SSH_TARGET}" "
    set -e
    test -d /opt/evo/bin || {
        echo 'FAIL: /opt/evo/bin missing on target (run prototype-install.sh first)' >&2
        exit 1
    }
    test -f /etc/systemd/system/evo.service || {
        echo 'FAIL: evo.service unit missing on target (run prototype-install.sh first)' >&2
        exit 1
    }
    test -f /etc/systemd/system/evo.service.d/exec-start.conf || {
        echo 'WARN: exec-start.conf drop-in missing; systemd may launch the framework default binary on next restart (run bootstrap.sh)' >&2
    }
    test -f /etc/systemd/system/evo.service.d/hardware-audio-privileges.conf || {
        echo 'WARN: hardware-audio-privileges.conf drop-in missing; hardware.audio select_dac / clear_dac / modder writes will fail with Read-only file system inside the service namespace (run bootstrap.sh)' >&2
    }
" >/dev/null 2>&1; then
    echo "FAIL: target pre-flight failed; cannot continue" >&2
    exit 1
fi
echo "  ok"
echo

# ----------------------------------------------------------
# [1/5] Cross-build the steward binary + every OOP plugin's
# wire binary listed in OOP_PLUGINS. Each build is invoked
# independently so a failure in one bundle's wire binary
# leaves the others' artefacts intact for inspection.
# ----------------------------------------------------------
echo "[1/5] cross-build ${DIST_CRATE} + OOP wire binaries for ${TARGET_TRIPLE} ..."
cd "${REPO_ROOT}"

CROSS_HELPER="${REPO_ROOT}/scripts/cross-build.sh"

run_cross_build() {
    # Forward args directly to the cross helper or cargo
    # fallback. Returns the build command's exit code.
    if [[ -x "${CROSS_HELPER}" ]]; then
        "${CROSS_HELPER}" "${TARGET_TRIPLE}" --release "$@" >/dev/null 2>&1
    else
        cargo build --release --target "${TARGET_TRIPLE}" "$@" >/dev/null 2>&1
    fi
}

if ! run_cross_build --features alsa-substrate -p "${DIST_CRATE}"; then
    echo "FAIL: steward cross-build exited non-zero" >&2
    exit 2
fi

LOCAL_BIN="${REPO_ROOT}/target/${TARGET_TRIPLE}/release/${DIST_BIN}"
if [[ ! -x "${LOCAL_BIN}" ]]; then
    echo "FAIL: expected steward binary missing at ${LOCAL_BIN}" >&2
    exit 2
fi
echo "  ok (steward: ${LOCAL_BIN})"

for entry in "${OOP_PLUGINS[@]}"; do
    IFS=':' read -r p_name p_crate p_wire p_features <<< "${entry}"
    if [[ -n "${p_features}" ]]; then
        if ! run_cross_build -p "${p_crate}" --bin "${p_wire}" --features "${p_features}"; then
            echo "FAIL: ${p_name} wire-binary cross-build exited non-zero" >&2
            exit 2
        fi
    else
        if ! run_cross_build -p "${p_crate}" --bin "${p_wire}"; then
            echo "FAIL: ${p_name} wire-binary cross-build exited non-zero" >&2
            exit 2
        fi
    fi
    p_local_bin="${REPO_ROOT}/target/${TARGET_TRIPLE}/release/${p_wire}"
    if [[ ! -x "${p_local_bin}" ]]; then
        echo "FAIL: expected wire binary missing at ${p_local_bin}" >&2
        exit 2
    fi
    echo "  ok (plugin ${p_name}: ${p_local_bin})"
done
echo

# ----------------------------------------------------------
# [2/5] Stop the steward; preserve previous binary.
# ----------------------------------------------------------
echo "[2/5] stop steward + preserve previous binary as evo-device-audio.prev ..."
if ! ssh "${SSH_TARGET}" "
    set -e
    sudo -n systemctl stop evo || true
    if [ -f ${TARGET_BIN_PATH} ]; then
        sudo -n cp ${TARGET_BIN_PATH} ${TARGET_BIN_PREV}
    fi
"; then
    echo "FAIL: could not stop steward on target" >&2
    exit 3
fi
echo "  ok"
echo

# ----------------------------------------------------------
# [3/5] scp the new steward binary + every OOP plugin bundle
# + install in place. Each plugin bundle is staged locally as
# a directory containing `manifest.toml` (renamed from the
# `manifest.oop.toml` template) and `plugin.bin` (the wire
# binary renamed per the manifest's `transport.exec`), then
# scp'd to a temp location and atomically promoted to
# `/opt/evo/plugins/<plugin-name>/`. The steward's Phase 2
# discovery walks `/opt/evo/plugins/` at boot and admits each
# bundle.
# ----------------------------------------------------------
echo "[3/5] scp + install steward binary + OOP plugin bundles ..."

# Sweep stale OOP bundle dirs on the target. Plugin dirs
# under `/opt/evo/plugins/` that are NOT in this deploy's
# `OOP_PLUGINS` array are stale — either from an earlier
# deploy that has since reverted a plugin to Phase 1
# compile-link admission, or from a plugin name that's been
# retired entirely. Leaving them in place causes the
# framework's Phase 2 discovery to attempt admission, fail
# (because the matching Phase 1 path admits the same plugin
# first, or because the bundle no longer matches the live
# plugin contract), and emit noisy `skipping plugin: admission
# failed` log lines on every boot.
EXPECTED_DIRS=""
for entry in "${OOP_PLUGINS[@]}"; do
    IFS=':' read -r p_name _ _ _ <<< "${entry}"
    EXPECTED_DIRS="${EXPECTED_DIRS} ${p_name}"
done
if ! ssh "${SSH_TARGET}" "
    set -e
    if [ -d /opt/evo/plugins ]; then
        for d in /opt/evo/plugins/*/; do
            name=\$(basename \"\$d\")
            case \" ${EXPECTED_DIRS} \" in
                *\" \$name \"*) ;;
                *) sudo -n rm -rf \"\$d\" && echo \"  swept stale: \$name\" ;;
            esac
        done
    fi
"; then
    echo "FAIL: stale bundle sweep on target failed" >&2
    exit 3
fi

TMP_REMOTE="/tmp/evo-device-audio.deploy.$$"
if ! scp -q "${LOCAL_BIN}" "${SSH_TARGET}:${TMP_REMOTE}"; then
    echo "FAIL: scp steward binary to target failed" >&2
    exit 3
fi
if ! ssh "${SSH_TARGET}" "
    set -e
    sudo -n install -m 0755 -o root -g root ${TMP_REMOTE} ${TARGET_BIN_PATH}
    rm -f ${TMP_REMOTE}
"; then
    echo "FAIL: install steward binary on target failed" >&2
    exit 3
fi
echo "  ok (steward installed)"

for entry in "${OOP_PLUGINS[@]}"; do
    IFS=':' read -r p_name p_crate p_wire _ <<< "${entry}"
    p_local_bin="${REPO_ROOT}/target/${TARGET_TRIPLE}/release/${p_wire}"
    p_manifest_src="${REPO_ROOT}/plugins/${p_name}/manifest.oop.toml"
    if [[ ! -f "${p_manifest_src}" ]]; then
        echo "FAIL: ${p_name} manifest.oop.toml missing at ${p_manifest_src}" >&2
        exit 3
    fi

    # Stage the bundle locally so the layout matches what the
    # framework's Phase 2 walks: <plugin-name>/{manifest.toml,
    # plugin.bin}. The staging dir is per-target-triple so
    # parallel deploys to different architectures don't trample
    # each other.
    p_stage_dir="${REPO_ROOT}/target/${TARGET_TRIPLE}/release/bundles/${p_name}"
    rm -rf "${p_stage_dir}"
    mkdir -p "${p_stage_dir}"
    cp "${p_manifest_src}" "${p_stage_dir}/manifest.toml"
    cp "${p_local_bin}" "${p_stage_dir}/plugin.bin"
    chmod 0755 "${p_stage_dir}/plugin.bin"

    # Sign the bundle. The framework's discovery refuses
    # unsigned bundles at admission with an explicit error;
    # signing is the only path to the trust class the
    # manifest declares. `evo-plugin-tool sign` writes
    # `manifest.sig` next to `manifest.toml`.
    if ! cargo run --quiet --release \
            --manifest-path "${ENG_ROOT}/Cargo.toml" \
            -p evo-plugin-tool -- \
            sign "${p_stage_dir}" --key "${EVO_PLUGIN_SIGNING_KEY}" \
            >/dev/null 2>&1; then
        echo "FAIL: signing ${p_name} bundle failed" >&2
        echo "      ran: cargo run --manifest-path ${ENG_ROOT}/Cargo.toml -p evo-plugin-tool -- sign ${p_stage_dir} --key ${EVO_PLUGIN_SIGNING_KEY}" >&2
        exit 3
    fi
    if [[ ! -f "${p_stage_dir}/manifest.sig" ]]; then
        echo "FAIL: ${p_stage_dir}/manifest.sig missing after sign" >&2
        exit 3
    fi

    p_remote_tmp="/tmp/evo-plugin-${p_name}.deploy.$$"
    if ! scp -qr "${p_stage_dir}" "${SSH_TARGET}:${p_remote_tmp}"; then
        echo "FAIL: scp ${p_name} bundle to target failed" >&2
        exit 3
    fi
    if ! ssh "${SSH_TARGET}" "
        set -e
        sudo -n mkdir -p /opt/evo/plugins/${p_name}
        sudo -n install -m 0644 -o root -g root \
            ${p_remote_tmp}/manifest.toml \
            /opt/evo/plugins/${p_name}/manifest.toml
        sudo -n install -m 0644 -o root -g root \
            ${p_remote_tmp}/manifest.sig \
            /opt/evo/plugins/${p_name}/manifest.sig
        sudo -n install -m 0755 -o root -g root \
            ${p_remote_tmp}/plugin.bin \
            /opt/evo/plugins/${p_name}/plugin.bin
        rm -rf ${p_remote_tmp}
    "; then
        echo "FAIL: install ${p_name} bundle on target failed" >&2
        exit 3
    fi
    echo "  ok (plugin ${p_name} installed)"
done
echo

# ----------------------------------------------------------
# [4/5] Start steward.
# ----------------------------------------------------------
echo "[4/5] start steward ..."
if ! ssh "${SSH_TARGET}" 'sudo -n systemctl start evo'; then
    echo "FAIL: systemctl start evo returned non-zero" >&2
    exit 4
fi
# Brief settle window before the verify probe.
sleep 3
echo "  ok"
echo

# ----------------------------------------------------------
# [5/5] Verify service is active + steward emitted ready.
# ----------------------------------------------------------
echo "[5/5] verify ..."
ACTIVE_STATE="$(ssh "${SSH_TARGET}" 'systemctl is-active evo' 2>/dev/null || true)"
if [[ "${ACTIVE_STATE}" != "active" ]]; then
    echo "FAIL: evo.service is not active (state=${ACTIVE_STATE})" >&2
    echo "      check 'journalctl -u evo --no-pager -n 80' on the target" >&2
    exit 4
fi
echo "  [ok]  evo.service active"

READY_HITS="$(ssh "${SSH_TARGET}" \
    'sudo -n journalctl -u evo --since "30 seconds ago" --no-pager 2>&1 \
        | grep -cE "evo ready|server listening|fast path listening"')"
if [[ "${READY_HITS}" -ge 1 ]]; then
    echo "  [ok]  steward emitted ready / listening signals (${READY_HITS} matching lines)"
else
    echo "  [WARN] no ready / listening signal in the last 30 s of evo journal"
    echo "         (check 'journalctl -u evo --no-pager -n 80' on the target)"
fi

# Verify every OOP plugin in the bundle set admitted through
# Phase 2 discovery on this boot. The framework's
# `plugin_discovery::discover_and_admit` emits a per-bundle
# admit line; presence of every plugin name in the recent
# journal confirms the bundle layout was correct and the
# admit handshake succeeded.
for entry in "${OOP_PLUGINS[@]}"; do
    IFS=':' read -r p_name _ _ <<< "${entry}"
    PLUGIN_HITS="$(ssh "${SSH_TARGET}" \
        "sudo -n journalctl -u evo --since '30 seconds ago' --no-pager 2>&1 \
            | grep -cE 'plugin.*${p_name}|${p_name}.*admit'")"
    if [[ "${PLUGIN_HITS}" -ge 1 ]]; then
        echo "  [ok]  plugin ${p_name} discovered + admitted (${PLUGIN_HITS} matching lines)"
    else
        echo "  [WARN] no discovery / admit signal for ${p_name} in the last 30 s"
        echo "         (check 'journalctl -u evo --no-pager -n 80' on the target)"
    fi
done

echo
echo "=== deploy-distribution.sh complete ==="
echo "Steward binary deployed to ${SSH_TARGET}:${TARGET_BIN_PATH}"
echo "Previous steward binary preserved at ${SSH_TARGET}:${TARGET_BIN_PREV}"
for entry in "${OOP_PLUGINS[@]}"; do
    IFS=':' read -r p_name _ _ <<< "${entry}"
    echo "OOP plugin bundle deployed to ${SSH_TARGET}:/opt/evo/plugins/${p_name}/"
done
echo "Rollback: ssh ${SSH_TARGET} 'sudo -n cp ${TARGET_BIN_PREV} ${TARGET_BIN_PATH} && sudo -n systemctl restart evo'"
