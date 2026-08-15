#!/usr/bin/env bash
# evo-install.sh — operator-facing online installer + reset
# primitives for evo-device-audio.
#
# The installer is the operator-visible entry point. It
# fetches a signed artefact bundle from the project's artefact
# channel, verifies the signature against a trust root pinned
# in this script, applies the bundle to the device as a
# single deterministic walk, and reports a structured
# post-condition. The operator runs ONE command; the existing
# bootstrap + deploy + reset walkers in `dist/scripts/`
# collapse into this surface for operator-facing flows.
#
# Four primitives, one per --mode value:
#
#   --mode=install
#       (default) Fresh device first install. Stages the
#       bundle, places /opt/evo, /etc/evo, sudoers, systemd
#       unit + drop-ins, ALSA + MPD reference configs,
#       creates the music library skeleton, starts the
#       steward, verifies the post-condition.
#
#   --mode=reinstall
#       Full wipe + reinstall. NOTHING survives the wipe —
#       /opt/evo, /etc/evo, sudoers entries, systemd unit
#       + drop-ins, /var/lib/evo (including the music
#       library), ALSA + MPD evo additions are all
#       removed. The host returns to its pre-evo state, then
#       the install flow runs. Post-condition identical to
#       --mode=install. Use when the operator wants a
#       guaranteed clean reset.
#
#   --mode=wipe-config
#       Wipe binaries + config + runtime state. PRESERVE the
#       music library. Re-fetches the bundle and re-applies
#       /opt/evo + /etc/evo + sudoers + drop-ins. Operator
#       config, admitted peers, groups, audio.options, and
#       every credential go back to installer-shipped
#       defaults. The music library content-hash list is
#       byte-equal before and after the operation.
#
#   --mode=wipe-user-data
#       Vacuum operator-generated state. PRESERVE binaries,
#       configs, sudoers, systemd, trust roots, asound,
#       mpd, AND the music library. The steward regenerates
#       its canonical_id, chain genesis, bootstrap token,
#       and signing keys on next start. Use when the
#       operator wants the device to behave as if first-
#       booting without re-installing.
#
# Channel selection:
#
#   EVO_BUNDLE_URL_BASE selects the artefact source. The
#   default points at the project's STABLE artefact channel.
#   Override the value to point at a developer-side HTTP
#   server hosting an unreleased bundle during release-cut
#   preparation.
#
# Usage:
#
#   curl -fsSL <URL>/install | sudo bash                       # default install
#   curl -fsSL <URL>/install -o evo-install.sh
#   sudo bash evo-install.sh --mode=install
#   sudo bash evo-install.sh --mode=reinstall
#   sudo bash evo-install.sh --mode=wipe-config
#   sudo bash evo-install.sh --mode=wipe-user-data
#
# Env tunables (apply across modes):
#   EVO_BUNDLE_URL_BASE         Channel-base URL.
#   EVO_BUNDLE_VERSION          Pin a specific bundle version.
#   EVO_BUNDLE_TRUST_ROOT_PEM   Override the pinned trust root
#                               (vendor-signed bundle).
#   EVO_SERVICE_USER            Service user (default: SUDO_USER
#                               or lowest non-system uid).
#   EVO_INSTALL_MUSIC_LIBRARY=0 Skip music-library skeleton at
#                               install time. Default: create.
#   EVO_ACCEPTANCE_SIGNING_KEY  Optional path to ed25519 PEM
#                               used to sign the emitted
#                               evidence record. Unsigned
#                               (placeholder) when absent.
#   EVO_INSTALL_EVIDENCE_OUT    Path to write the primitive's
#                               evidence record. Default:
#                               /var/lib/evo/evidence/
#                               <primitive>-<arch>.toml
#                               (created if absent).
#
# Exit codes:
#   0 — primitive succeeded; post-condition verified.
#   1 — operator error (wrong invocation, missing prerequisite,
#       no sudo).
#   2 — fetch error (bundle URL unreachable, signature file
#       missing).
#   3 — signature verification failed.
#   4 — apply error (a stage on the target failed).
#   5 — post-condition verification failed (service did not
#       become active, plugin failed to admit, music-library
#       hash diverged on a preserve-music primitive, etc.).

set -euo pipefail

# -------- Pinned trust root --------
#
# Public component of the ed25519 key the bundle was signed
# with. The bundle's signature must verify against this
# value or the install refuses. Operators who want to
# install a vendor-signed bundle override
# EVO_BUNDLE_TRUST_ROOT_PEM with their own pinned key.
EVO_BUNDLE_TRUST_ROOT_PEM_DEFAULT="-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAvJqIhluihUhLY435rJZnIjskDS9affTKSDUIYVIjVE0=
-----END PUBLIC KEY-----"
EVO_BUNDLE_TRUST_ROOT_PEM="${EVO_BUNDLE_TRUST_ROOT_PEM:-${EVO_BUNDLE_TRUST_ROOT_PEM_DEFAULT}}"

# -------- Defaults --------
# Default URL points at the public artefact channel for the
# stable distribution. Set EVO_BUNDLE_URL_BASE to override
# (e.g. point at a developer-side HTTP server hosting an
# unreleased bundle during release-cut preparation).
EVO_BUNDLE_URL_BASE="${EVO_BUNDLE_URL_BASE:-https://github.com/foonerd/evo-device-audio-artefacts/releases/latest/download}"
EVO_BUNDLE_VERSION="${EVO_BUNDLE_VERSION:-0.1.0}"
EVO_INSTALL_MUSIC_LIBRARY="${EVO_INSTALL_MUSIC_LIBRARY:-1}"
EVO_INSTALL_EVIDENCE_OUT="${EVO_INSTALL_EVIDENCE_OUT:-}"
EVO_ACCEPTANCE_SIGNING_KEY="${EVO_ACCEPTANCE_SIGNING_KEY:-}"

# -------- Argument parsing --------
MODE="install"
# Flags relayed to bootstrap.sh's placement primitive. evo-
# install.sh delegates ALL /etc placement (asound.conf,
# sudoers, systemd drop-ins, mpd include, plugins.d defaults,
# trust roots, asound.d, modder dirs) to bootstrap.sh — these
# variables travel with the call. Empty values are dropped at
# call time; bootstrap.sh applies its own per-flag defaults.
EVO_INSTALL_AUDIO_CARD=""
MULTIROOM_ROLE=""
MULTIROOM_GROUP_ID=""
MULTIROOM_SOURCE_PCM=""
MULTIROOM_ALSA_PCM=""
MULTIROOM_GROUP_MEMBERS=""
MULTIROOM_GROUP_MEMBER_ADDRESSES=""
print_usage() {
    sed -n '2,90p' "$0" >&2
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --mode=*)
            MODE="${1#--mode=}"
            shift
            ;;
        --mode)
            if [[ $# -lt 2 ]]; then
                echo "FAIL: --mode requires a value" >&2
                exit 1
            fi
            MODE="$2"
            shift 2
            ;;
        --card)
            EVO_INSTALL_AUDIO_CARD="$2" ; shift 2 ;;
        --card=*)
            EVO_INSTALL_AUDIO_CARD="${1#--card=}" ; shift ;;
        --multiroom-role)
            MULTIROOM_ROLE="$2" ; shift 2 ;;
        --multiroom-role=*)
            MULTIROOM_ROLE="${1#--multiroom-role=}" ; shift ;;
        --multiroom-group-id)
            MULTIROOM_GROUP_ID="$2" ; shift 2 ;;
        --multiroom-group-id=*)
            MULTIROOM_GROUP_ID="${1#--multiroom-group-id=}" ; shift ;;
        --multiroom-source-pcm)
            MULTIROOM_SOURCE_PCM="$2" ; shift 2 ;;
        --multiroom-source-pcm=*)
            MULTIROOM_SOURCE_PCM="${1#--multiroom-source-pcm=}" ; shift ;;
        --multiroom-alsa-pcm)
            MULTIROOM_ALSA_PCM="$2" ; shift 2 ;;
        --multiroom-alsa-pcm=*)
            MULTIROOM_ALSA_PCM="${1#--multiroom-alsa-pcm=}" ; shift ;;
        --multiroom-group-members)
            MULTIROOM_GROUP_MEMBERS="$2" ; shift 2 ;;
        --multiroom-group-members=*)
            MULTIROOM_GROUP_MEMBERS="${1#--multiroom-group-members=}" ; shift ;;
        --multiroom-group-member-addresses)
            MULTIROOM_GROUP_MEMBER_ADDRESSES="$2" ; shift 2 ;;
        --multiroom-group-member-addresses=*)
            MULTIROOM_GROUP_MEMBER_ADDRESSES="${1#--multiroom-group-member-addresses=}" ; shift ;;
        -h|--help)
            print_usage
            exit 0
            ;;
        *)
            echo "FAIL: unknown argument: $1" >&2
            print_usage
            exit 1
            ;;
    esac
done

case "${MODE}" in
    install|reinstall|wipe-config|wipe-user-data) ;;
    *)
        echo "FAIL: invalid --mode='${MODE}' (expected install|reinstall|wipe-config|wipe-user-data)" >&2
        exit 1
        ;;
esac

PRIMITIVE_ID=""
case "${MODE}" in
    install)         PRIMITIVE_ID="p1_full_initial_setup" ;;
    reinstall)       PRIMITIVE_ID="p2_full_wipe_and_reinstall" ;;
    wipe-config)     PRIMITIVE_ID="p3_config_wipe_preserving_music" ;;
    wipe-user-data)  PRIMITIVE_ID="p4_user_data_full_vacuum" ;;
esac

# -------- Pre-flight: hostname sanity --------
# Every device-identity surface on the LAN — the SMB `netbios
# name` in `/etc/samba/smb.conf`, the mDNS instance name, the
# UI's "Device name" field — derives from the OS hostname
# (`/proc/sys/kernel/hostname`). A fleet of devices that all
# ship with the same generic image-baked hostname
# (`raspberrypi`, `debian`, `nuc`, `localhost`) collide on the
# subnet: NetBIOS name registration refuses the second and
# third to try. Refuse to proceed if the hostname is generic;
# the operator sets a unique one via `hostnamectl set-hostname
# <name>` before re-running. `EVO_INSTALL_ALLOW_GENERIC_HOSTNAME=1`
# is an explicit override for image-build and CI paths that
# know they'll set the hostname later; it MUST NEVER be set
# on a device the operator is bringing up.
current_hostname="$(cat /proc/sys/kernel/hostname 2>/dev/null || true)"
case "${current_hostname}" in
    ""|localhost|localhost.localdomain|raspberrypi|debian|ubuntu|nuc)
        if [[ "${EVO_INSTALL_ALLOW_GENERIC_HOSTNAME:-0}" != "1" ]]; then
            echo "FAIL: hostname is generic ('${current_hostname}')." >&2
            echo "      Every device-identity surface (SMB netbios name," >&2
            echo "      mDNS instance name, UI 'Device name') derives from" >&2
            echo "      /proc/sys/kernel/hostname. A fleet of devices with the" >&2
            echo "      same generic hostname collides on the LAN — the first" >&2
            echo "      to register wins, the rest go invisible." >&2
            echo "" >&2
            echo "      Set a unique hostname first, then re-run:" >&2
            echo "        sudo hostnamectl set-hostname <unique-name>" >&2
            echo "" >&2
            echo "      Then:" >&2
            echo "        sudo bash $0 --mode=${MODE}" >&2
            exit 1
        fi
        echo "WARN: proceeding with generic hostname='${current_hostname}'" >&2
        echo "      because EVO_INSTALL_ALLOW_GENERIC_HOSTNAME=1 is set." >&2
        echo "      Set a real hostname before the device joins any LAN." >&2
        ;;
esac

# -------- Pre-flight: root --------
if [[ "$(id -u)" -ne 0 ]]; then
    echo "FAIL: evo-install.sh must run as root (sudo bash $0)" >&2
    exit 1
fi

# -------- Pre-flight: required tools --------
need_tool() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "FAIL: required tool missing: $1" >&2
        exit 1
    fi
}
need_tool curl
need_tool tar
need_tool gzip
need_tool openssl
need_tool sha256sum
need_tool systemctl
need_tool install
need_tool find
need_tool sed

# -------- Resolve service user --------
SERVICE_USER="${EVO_SERVICE_USER:-}"
if [[ -z "${SERVICE_USER}" && -n "${SUDO_USER:-}" && "${SUDO_USER}" != "root" ]]; then
    SERVICE_USER="${SUDO_USER}"
fi
if [[ -z "${SERVICE_USER}" ]]; then
    SERVICE_USER="$(awk -F: '$3 >= 1000 && $3 < 65000 {print $1; exit}' /etc/passwd)"
fi
if [[ -z "${SERVICE_USER}" ]] || ! id "${SERVICE_USER}" >/dev/null 2>&1; then
    echo "FAIL: could not resolve service user (set EVO_SERVICE_USER=<name>)" >&2
    exit 1
fi

# -------- Detect architecture --------
case "$(uname -m)" in
    x86_64) ARCH="x86_64-unknown-linux-gnu" ;;
    aarch64) ARCH="aarch64-unknown-linux-gnu" ;;
    armv7l) ARCH="armv7-unknown-linux-gnueabihf" ;;
    *)
        echo "FAIL: unsupported architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

# -------- Bundle path resolution --------
BUNDLE_NAME="evo-device-audio-${ARCH}-${EVO_BUNDLE_VERSION}.tar.gz"
BUNDLE_URL="${EVO_BUNDLE_URL_BASE}/${BUNDLE_NAME}"
BUNDLE_SIG_URL="${BUNDLE_URL}.sig"

START_NS="$(date -u +%s%N)"
START_UTC="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

echo "=== evo-install.sh ==="
echo "Mode:          ${MODE} (${PRIMITIVE_ID})"
echo "Architecture:  ${ARCH}"
echo "Bundle URL:    ${BUNDLE_URL}"
echo "Service user:  ${SERVICE_USER}"
echo "Music library: $([[ ${EVO_INSTALL_MUSIC_LIBRARY} != 0 ]] && echo create-or-preserve || echo skip)"
echo ""

# -------- Pre-flight: install system packages --------
#
# Compulsory OS-dependency provisioning. The framework's install
# contract (evo-plugin-tool install + steward admission) both
# enforce the parity gate: has_os_dependencies=true + any absent
# required_binary = HARD FAIL. This distribution installer is
# the Strategy A owner that BRINGS the packages so admission and
# tool-install both find every binary present. Absence of a
# required binary after apt-install runs is a failed install.
#
# The mapping below mirrors the SDK's binary→package registry
# (`evo_plugin_sdk::privileges::DistroFamily::binary_to_package`).
# Keep the two in sync when adding a new binary.

debian_package_for_binary() {
    case "$1" in
        aplay|amixer)        echo "alsa-utils" ;;
        mpc)                 echo "mpc" ;;
        mpd)                 echo "mpd" ;;
        curl)                echo "curl" ;;
        ffmpeg)              echo "ffmpeg" ;;
        systemctl)           echo "systemd" ;;
        nmcli)               echo "network-manager" ;;
        iw)                  echo "iw" ;;
        rfkill)              echo "rfkill" ;;
        smbclient)           echo "smbclient" ;;
        mount.cifs)          echo "cifs-utils" ;;
        mount.nfs)           echo "nfs-common" ;;
        avahi-browse)        echo "avahi-utils" ;;
        smbd)                echo "samba" ;;
        testparm|smbpasswd)  echo "samba-common-bin" ;;
        sudo)                echo "sudo" ;;
        tee|cat)             echo "coreutils" ;;
        # storage.usb plugin: block-device enumeration + repair
        # matrix. bootstrap.sh Step 1g apt-installs the same set
        # at install time; these mappings let the installer's
        # per-plugin package-parity check see them.
        lsblk|findmnt|blockdev)    echo "util-linux" ;;
        fsck.vfat)                 echo "dosfstools" ;;
        fsck.exfat)                echo "exfatprogs" ;;
        ntfsfix)                   echo "ntfs-3g" ;;
        e2fsck)                    echo "e2fsprogs" ;;
        eject)                     echo "eject" ;;
        evo-*)               echo "PLUGIN_PROVIDED" ;;
        *)                   echo "" ;;
    esac
}

# Extract every required_binary name from a plugin's privileges.yaml.
# Reads the block bounded by ^required_binaries: through the next
# top-level key. Emits one binary name per line; empty output when
# the plugin has an empty required_binaries vector.
extract_required_binaries() {
    local yaml_path="$1"
    awk '
        /^required_binaries:/ { in_block=1; next }
        in_block && /^[a-z_][a-z_0-9]*:/ && !/^required_binaries:/ { in_block=0 }
        in_block && /^[[:space:]]*-[[:space:]]*name:/ {
            sub(/^[[:space:]]*-[[:space:]]*name:[[:space:]]*/, "")
            gsub(/[[:space:]]+$/, "")
            print
        }
    ' "$yaml_path"
}

# Read has_os_dependencies from a privileges.yaml. Emits `true`,
# `false`, or empty (schema-invalid record; treated as failure by
# the caller).
extract_has_os_dependencies() {
    local yaml_path="$1"
    awk '
        /^has_os_dependencies:[[:space:]]*/ {
            sub(/^has_os_dependencies:[[:space:]]*/, "")
            gsub(/[[:space:]]+$/, "")
            print
            exit
        }
    ' "$yaml_path"
}

# Extract every required_system_services unit name from a
# plugin's privileges.yaml. Reads the block bounded by
# ^required_system_services: through the next top-level key.
# Emits one unit name per line; empty output when the plugin
# has an empty required_system_services vector. Recognises the
# YAML shape `- unit.service` (list of strings, per the
# privileges v1.0 schema).
extract_required_system_services() {
    local yaml_path="$1"
    awk '
        /^required_system_services:/ { in_block=1; next }
        in_block && /^[a-z_][a-z_0-9]*:/ && !/^required_system_services:/ { in_block=0 }
        in_block && /^[[:space:]]*-[[:space:]]/ {
            sub(/^[[:space:]]*-[[:space:]]+/, "")
            gsub(/[[:space:]]+$/, "")
            gsub(/[\"'\'']/, "")
            print
        }
    ' "$yaml_path"
}

ensure_system_packages() {
    # Baseline packages the reference distribution needs regardless
    # of which plugins are bundled: the framework depends on
    # network-manager for LAN control regardless of the network
    # plugin's admission, and mpd + mpc + alsa-utils are the
    # audio-reference bundle's steady-state runtime.
    local pkgs_needed=()
    local pkg
    for pkg in mpd alsa-utils mpc network-manager; do
        if ! dpkg -s "${pkg}" >/dev/null 2>&1; then
            pkgs_needed+=("${pkg}")
        fi
    done

    # Walk every bundled plugin's privileges.yaml. For each plugin
    # that declares has_os_dependencies:true, resolve every
    # required_binaries[].name → providing package via the mapping
    # above and add unique packages to pkgs_needed. Refuse install
    # HARD if any plugin's privileges.yaml is unreadable or its
    # has_os_dependencies flag is absent — the SDK requires the
    # field and the parity gate reads it first.
    local plugin_dir yaml_path plugin_id has_deps binary pkg_name
    if [[ -d "${STAGE_DIR}/plugins" ]]; then
        for plugin_dir in "${STAGE_DIR}/plugins/"*/; do
            [[ -d "${plugin_dir}" ]] || continue
            # Skip non-bundle plugin subtrees: some in-process
            # plugins (network.shares, network.smb-server) live
            # under `plugins/<id>/dist/` for their distribution-
            # tier artefacts (wrappers + sudoers templates) but
            # do NOT ship an OOP bundle (no manifest.toml + no
            # plugin.bin at the plugin dir root). Only walk
            # directories that carry a manifest — those are the
            # OOP bundles the preflight is meant to enforce.
            if [[ ! -f "${plugin_dir}manifest.toml" ]]; then
                continue
            fi
            yaml_path="${plugin_dir}privileges.yaml"
            plugin_id="$(basename "${plugin_dir%/}")"
            if [[ ! -f "${yaml_path}" ]]; then
                echo "FAIL: plugin ${plugin_id} missing privileges.yaml — every bundled plugin MUST ship its privileges contract" >&2
                exit 5
            fi
            has_deps="$(extract_has_os_dependencies "${yaml_path}")"
            if [[ -z "${has_deps}" ]]; then
                echo "FAIL: plugin ${plugin_id} privileges.yaml has no has_os_dependencies field (compulsory per schema v1.0)" >&2
                exit 5
            fi
            if [[ "${has_deps}" != "true" ]]; then
                continue
            fi
            while IFS= read -r binary; do
                [[ -n "${binary}" ]] || continue
                pkg_name="$(debian_package_for_binary "${binary}")"
                if [[ -z "${pkg_name}" ]]; then
                    echo "FAIL: plugin ${plugin_id} declares required_binary '${binary}' with no debian package mapping in evo-install.sh — extend debian_package_for_binary() before shipping" >&2
                    exit 5
                fi
                # PLUGIN_PROVIDED is the sentinel for binaries the
                # plugin's own dist tree installs (typically at
                # /usr/local/bin/), not from an external apt
                # package. Bootstrap places them during Step 3
                # apply; admission-time PPAG re-verifies. Skip the
                # apt-install accumulator so we don't try to fetch
                # a package that does not exist upstream.
                if [[ "${pkg_name}" == "PLUGIN_PROVIDED" ]]; then
                    continue
                fi
                if ! dpkg -s "${pkg_name}" >/dev/null 2>&1; then
                    if ! printf '%s\n' "${pkgs_needed[@]}" | grep -qxF "${pkg_name}"; then
                        pkgs_needed+=("${pkg_name}")
                    fi
                fi
            done < <(extract_required_binaries "${yaml_path}")
        done
    fi

    if [[ ${#pkgs_needed[@]} -gt 0 ]]; then
        echo "  installing: ${pkgs_needed[*]}"
        DEBIAN_FRONTEND=noninteractive apt-get update -qq
        if ! DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${pkgs_needed[@]}"; then
            echo "FAIL: apt-get install of ${pkgs_needed[*]} did not succeed — plugin prerequisites are unmet; refusing to promote a bundle whose admission would fail" >&2
            exit 6
        fi
    fi

    # Enable + start every declared required_system_services unit.
    # A plugin that declares `avahi-daemon.service` and shells out
    # to `avahi-browse` cannot succeed if the daemon is not
    # running, even when the binary is present. Symmetric with
    # the steward's admission-time parity gate: install brings the
    # units up; admission refuses if the units are down.
    local unit
    if [[ -d "${STAGE_DIR}/plugins" ]]; then
        for plugin_dir in "${STAGE_DIR}/plugins/"*/; do
            [[ -d "${plugin_dir}" ]] || continue
            # Skip non-bundle plugin subtrees (in-process plugins
            # whose only bundle-side presence is a `dist/`
            # sub-tree with wrappers + sudoers templates).
            [[ -f "${plugin_dir}manifest.toml" ]] || continue
            yaml_path="${plugin_dir}privileges.yaml"
            plugin_id="$(basename "${plugin_dir%/}")"
            has_deps="$(extract_has_os_dependencies "${yaml_path}")"
            [[ "${has_deps}" == "true" ]] || continue
            while IFS= read -r unit; do
                [[ -n "${unit}" ]] || continue
                if ! systemctl enable --now "${unit}" >/dev/null 2>&1; then
                    echo "FAIL: plugin ${plugin_id} declares required_system_service '${unit}' but systemctl enable --now failed — plugin cannot admit; refusing to proceed" >&2
                    systemctl status --no-pager "${unit}" >&2 || true
                    exit 8
                fi
            done < <(extract_required_system_services "${yaml_path}")
        done
    fi

    # Post-install verification: after apt-install + systemctl
    # enable --now runs, every plugin that declared
    # has_os_dependencies:true MUST see every required_binary on
    # PATH AND every required_system_services unit active. This
    # closes the parity contract on the distribution-installer
    # side. Steward admission runs the same gate independently as
    # a symmetric check.
    if [[ -d "${STAGE_DIR}/plugins" ]]; then
        for plugin_dir in "${STAGE_DIR}/plugins/"*/; do
            [[ -d "${plugin_dir}" ]] || continue
            [[ -f "${plugin_dir}manifest.toml" ]] || continue
            yaml_path="${plugin_dir}privileges.yaml"
            plugin_id="$(basename "${plugin_dir%/}")"
            has_deps="$(extract_has_os_dependencies "${yaml_path}")"
            [[ "${has_deps}" == "true" ]] || continue
            while IFS= read -r binary; do
                [[ -n "${binary}" ]] || continue
                # PLUGIN_PROVIDED wrappers (evo-smb-user-sync,
                # evo-usb-mount, …) are installed by bootstrap
                # Step 1* which runs at install Step 6 — after
                # this parity check. Skip them here; the
                # steward's admission-time PPAG at Step 7 covers
                # them symmetrically once bootstrap has placed
                # the wrapper on PATH.
                local pkg_name
                pkg_name="$(debian_package_for_binary "${binary}")"
                if [[ "${pkg_name}" == "PLUGIN_PROVIDED" ]]; then
                    continue
                fi
                if ! command -v "${binary}" >/dev/null 2>&1; then
                    echo "FAIL: plugin ${plugin_id} declares required_binary '${binary}' but it is absent from PATH after apt-install — the parity gate would refuse admission; refusing to proceed" >&2
                    exit 7
                fi
            done < <(extract_required_binaries "${yaml_path}")
            while IFS= read -r unit; do
                [[ -n "${unit}" ]] || continue
                if ! systemctl is-active --quiet "${unit}"; then
                    echo "FAIL: plugin ${plugin_id} declares required_system_service '${unit}' but it is not active after enable --now — the parity gate would refuse admission; refusing to proceed" >&2
                    exit 9
                fi
            done < <(extract_required_system_services "${yaml_path}")
        done
    fi
}

# -------- Stage dir lifecycle --------
WORK_DIR=""
STAGE_DIR=""
init_work_dir() {
    WORK_DIR="$(mktemp -d -t evo-install.XXXXXX)"
    STAGE_DIR="${WORK_DIR}/stage"
    mkdir -p "${STAGE_DIR}"
    trap 'rm -rf "${WORK_DIR}"' EXIT
}

# -------- Fetch + verify the bundle --------
BUNDLE_PATH=""
BUNDLE_SIG_PATH=""
BUNDLE_SHA256=""
BUNDLE_SIZE=0

fetch_and_verify_bundle() {
    BUNDLE_PATH="${WORK_DIR}/${BUNDLE_NAME}"
    BUNDLE_SIG_PATH="${BUNDLE_PATH}.sig"
    local trust_root_path="${WORK_DIR}/trust-root.pem"
    if ! curl -fsSL --connect-timeout 10 --max-time 600 -o "${BUNDLE_PATH}" "${BUNDLE_URL}"; then
        echo "FAIL: fetch ${BUNDLE_URL} failed" >&2
        exit 2
    fi
    if ! curl -fsSL --connect-timeout 10 --max-time 60 -o "${BUNDLE_SIG_PATH}" "${BUNDLE_SIG_URL}"; then
        echo "FAIL: fetch ${BUNDLE_SIG_URL} failed" >&2
        exit 2
    fi
    BUNDLE_SHA256="$(sha256sum "${BUNDLE_PATH}" | awk '{print $1}')"
    BUNDLE_SIZE="$(stat -c %s "${BUNDLE_PATH}")"
    printf '%s\n' "${EVO_BUNDLE_TRUST_ROOT_PEM}" > "${trust_root_path}"
    if ! openssl pkeyutl -verify \
            -pubin -inkey "${trust_root_path}" \
            -rawin -in "${BUNDLE_PATH}" \
            -sigfile "${BUNDLE_SIG_PATH}" >/dev/null 2>&1; then
        echo "FAIL: signature does not verify against pinned trust root" >&2
        exit 3
    fi
}

extract_bundle() {
    tar -C "${STAGE_DIR}" -xzf "${BUNDLE_PATH}"
    if [[ ! -f "${STAGE_DIR}/bundle-manifest.toml" ]]; then
        echo "FAIL: bundle missing bundle-manifest.toml" >&2
        exit 4
    fi
    if [[ ! -x "${STAGE_DIR}/bin/evo-device-audio" ]]; then
        echo "FAIL: bundle missing bin/evo-device-audio" >&2
        exit 4
    fi
}

# -------- Music library hash discipline --------
snapshot_music_hashes() {
    if [[ -d /var/lib/evo/music ]]; then
        find /var/lib/evo/music -type f -print0 2>/dev/null \
            | sort -z | xargs -0 -r sha256sum 2>/dev/null \
            | sha256sum | awk '{print $1}'
    else
        echo "no_music_library"
    fi
}

# -------- Wipe primitives --------
restore_pre_evo_asound_conf() {
    # Restore the oldest pre-evo backup so the original
    # operator-set asound.conf survives the install round-trip.
    # Backups are named with a timestamp suffix (pre-evo.<YYYYmmddHHMMSS>),
    # so bash's default lexicographic glob order is chronological;
    # element 0 is the oldest.
    local oldest_backup=""
    shopt -s nullglob
    local backups=(/etc/asound.conf.pre-evo.*)
    shopt -u nullglob
    if (( ${#backups[@]} > 0 )); then
        oldest_backup="${backups[0]}"
    fi
    if [[ -n "${oldest_backup}" && -f "${oldest_backup}" ]]; then
        mv "${oldest_backup}" /etc/asound.conf
    else
        rm -f /etc/asound.conf
    fi
    # Clean up any remaining backups.
    shopt -s nullglob
    local remaining=(/etc/asound.conf.pre-evo.*)
    shopt -u nullglob
    if (( ${#remaining[@]} > 0 )); then
        rm -f "${remaining[@]}"
    fi
}

strip_evo_include_from_mpd_conf() {
    if [[ ! -f /etc/mpd.conf ]]; then
        return 0
    fi
    # Restore the pre-evo /etc/mpd.conf wholesale if a backup
    # from the original music_directory rewrite exists. The
    # backup is a single, fixed-name file created by
    # `sed -i.pre-evo-music`; restoring it reverses the
    # music_directory edit and any include lines added since.
    if [[ -f /etc/mpd.conf.pre-evo-music ]]; then
        mv /etc/mpd.conf.pre-evo-music /etc/mpd.conf
    fi
    # Even after the restore, additional include lines may
    # have been added by other paths (bootstrap.sh, earlier
    # evo-install.sh variants). Purge them too.
    purge_evo_mpd_includes
}

stop_prior_steward() {
    # Stop the whole operator surface before any wipe of
    # /opt/evo. evo-ui.service writes StandardOutput to
    # /opt/evo/ui/logs/runtime.log; if the unit stays enabled
    # while wipe_full removes that directory, systemd
    # restart-loops with status=209/STDOUT (hundreds of
    # failures per minute — the 2026-08-07 fleet incident).
    # Same class for evo-kiosk which depends on evo-ui.
    systemctl stop evo-kiosk 2>/dev/null || true
    systemctl stop evo-ui 2>/dev/null || true
    systemctl stop evo 2>/dev/null || true
    # Reset any auto-restart-pending state from a previously
    # broken unit (e.g. an earlier install attempt that left
    # the unit transitionally without ExecStart). Without
    # this, systemd keeps logging "Service has no ExecStart=,
    # ExecStop=, or SuccessAction=. Refusing." while it
    # auto-retries during the install transition.
    systemctl reset-failed evo evo-ui evo-kiosk 2>/dev/null || true
    # Kill any evo-device-audio process not under systemd's
    # control (manual sudo launches survive systemctl stop).
    pkill -KILL -f '/opt/evo/bin/evo-device-audio' 2>/dev/null || true
    pkill -KILL -f '/opt/evo/plugins/.*/plugin\.bin' 2>/dev/null || true
    pkill -KILL -f '/opt/evo/bin/evo-ui-runtime' 2>/dev/null || true
}

wipe_full() {
    stop_prior_steward
    # Unmount every mount under /var/lib/evo before rm -rf.
    # The network.shares plugin mounts remote CIFS/NFS shares at
    # /var/lib/evo/music/NAS/<alias>; those mounts are typically
    # read-only from the remote side so rm -rf on the mount
    # itself fails with EROFS and, under `set -e`, aborts the
    # whole wipe. Explicit umount-first is idempotent (no-op when
    # nothing is mounted) and closes the failure class before it
    # can strand the device between "old evo gone" and "new evo
    # not yet installed".
    unmount_under_evo_state
    rm -rf /opt/evo
    rm -rf /etc/evo
    rm -f /etc/sudoers.d/evo-* 2>/dev/null || true
    rm -f /etc/systemd/system/evo.service
    rm -rf /etc/systemd/system/evo.service.d
    # Companion units also own paths under /opt/evo and
    # /var/lib/evo. Drop them with the wipe so a half-applied
    # previous install cannot restart into a missing tree.
    rm -f /etc/systemd/system/evo-ui.service
    rm -rf /etc/systemd/system/evo-ui.service.d
    rm -f /etc/systemd/system/evo-kiosk.service
    rm -rf /etc/systemd/system/evo-kiosk.service.d
    systemctl disable evo-ui.service evo-kiosk.service 2>/dev/null || true
    rm -rf /var/lib/evo
    # Music destruction must clear MPD durable curation that
    # lives OUTSIDE /var/lib/evo. Otherwise favourites /
    # playlists / tag_cache / queue state survive p2 and the
    # UI shows ghost library rows (2026-08-08 Gone-parity gap).
    reset_mpd_curation_after_music_wipe
    restore_pre_evo_asound_conf
    strip_evo_include_from_mpd_conf
    systemctl daemon-reload
}

# Reset MPD state that references wiped local music.
# Idempotent. Leaves mpd stopped — bootstrap recreates the
# music triad then restarts mpd against a clean DB.
reset_mpd_curation_after_music_wipe() {
    systemctl stop mpd 2>/dev/null || true
    systemctl reset-failed mpd 2>/dev/null || true
    # Song index + player state.
    rm -f /var/lib/mpd/tag_cache \
          /var/lib/mpd/state \
          /var/lib/mpd/sticker.sql \
          /var/lib/mpd/sticker.sql-journal \
          /var/lib/mpd/sticker.sql-wal \
          /var/lib/mpd/sticker.sql-shm 2>/dev/null || true
    # Stored playlists including __favourites__.
    if [[ -d /var/lib/mpd/playlists ]]; then
        find /var/lib/mpd/playlists -mindepth 1 -maxdepth 1 \
            -exec rm -rf -- {} + 2>/dev/null || true
    fi
    echo "[wipe] reset /var/lib/mpd curation (tag_cache/state/stickers/playlists)"
}

# Unmount every mount whose target sits under /var/lib/evo,
# deepest first (so nested mounts unmount before their parents).
# Idempotent: no-op when /var/lib/evo has no mounts.
unmount_under_evo_state() {
    local mounts
    mounts="$(findmnt -rno TARGET | grep '^/var/lib/evo' || true)"
    if [[ -z "${mounts}" ]]; then
        return 0
    fi
    # Sort by path depth desc so children unmount before parents.
    while IFS= read -r mnt; do
        [[ -n "${mnt}" ]] || continue
        # umount -R handles nested mounts atomically per subtree.
        # Fall back to lazy unmount on EBUSY — the wipe is the
        # last thing this process does with the tree, so
        # deferred cleanup of open descriptors is acceptable.
        umount -R "${mnt}" 2>/dev/null \
            || umount -l "${mnt}" 2>/dev/null \
            || true
    done < <(printf '%s\n' "${mounts}" \
                 | awk '{ print length($0), $0 }' \
                 | sort -nr \
                 | awk '{ $1=""; sub(/^ /,""); print }')
}

wipe_config() {
    stop_prior_steward
    rm -rf /opt/evo
    rm -rf /etc/evo
    rm -f /etc/sudoers.d/evo-* 2>/dev/null || true
    rm -rf /etc/systemd/system/evo.service.d
    # Preserve /etc/systemd/system/evo.service (installer
    # re-applies it idempotently).
    # Wipe /var/lib/evo/* EXCEPT /var/lib/evo/music.
    if [[ -d /var/lib/evo ]]; then
        find /var/lib/evo -mindepth 1 -maxdepth 1 -not -name music -exec rm -rf -- {} +
    fi
    systemctl daemon-reload
}

wipe_user_data() {
    stop_prior_steward
    # Vacuum operator-generated state subdirs. Preserve
    # binaries, configs, sudoers, systemd, trust roots,
    # asound, mpd, music.
    rm -rf /var/lib/evo/state
    rm -rf /var/lib/evo/plugins
    rm -rf /var/lib/evo/https/credentials
    rm -rf /var/lib/evo/plans
    # The /etc/evo baseline re-application happens via
    # invoke_bootstrap_placement after the bundle is extracted.
    rm -rf /etc/evo
}

# -------- Apply (factored install stages) --------
place_opt_evo() {
    install -d -m 0755 -o root -g root /opt/evo /opt/evo/bin /opt/evo/plugins /opt/evo/catalogue
    install -m 0755 -o root -g root \
        "${STAGE_DIR}/bin/evo-device-audio" \
        /opt/evo/bin/evo-device-audio
    # UI shell + first-run setup overlay. The framework's HTTPS
    # substrate reads static assets from `EVO_HTTPS_STATIC_DIR`
    # (pinned to `/opt/evo/ui` by
    # `dist/systemd/evo.service.d/https.conf`). Without the SPA
    # here, the steward logs "EVO_HTTPS_STATIC_DIR points at a
    # path that does not exist or is not a directory;
    # static-asset serving disabled" and the operator UI is
    # unreachable at https://<device>:8443/. The bundle stages
    # the built SPA at `<bundle>/ui/`; install it verbatim to
    # the STATIC_DIR target.
    if [[ -d "${STAGE_DIR}/ui" ]]; then
        # UI SPA lands under a release-timestamped subdirectory,
        # then `current` becomes a symlink to it. The framework's
        # own HTTPS listener reads from `/opt/evo/ui` per
        # https.conf, so also stage a top-level copy for the
        # framework-side static-asset serving. evo-ui-runtime
        # (the operator-facing HTTP/HTTPS listener on 80/443)
        # resolves `active_release=/opt/evo/ui/current`.
        install -d -m 0755 -o root -g root /opt/evo/ui
        install -d -m 0755 -o "${SERVICE_USER}" -g "${SERVICE_USER}" \
            /opt/evo/ui/releases /opt/evo/ui/data /opt/evo/ui/logs
        local release_id
        release_id="$(date -u +%Y%m%dT%H%M%SZ)"
        local release_dir="/opt/evo/ui/releases/${release_id}"
        install -d -m 0755 -o "${SERVICE_USER}" -g "${SERVICE_USER}" \
            "${release_dir}"
        cp -a "${STAGE_DIR}/ui/." "${release_dir}/"
        # Point `current` at this release atomically. Idempotent
        # under repeated install runs — ln -sfn replaces an
        # existing symlink in place without leaving a stale dir.
        ln -sfn "${release_dir}" /opt/evo/ui/current
        # Top-level copy for the framework's `EVO_HTTPS_STATIC_DIR=/opt/evo/ui`
        # static-asset serving path. Both surfaces resolve to
        # the same content, from one bundled source.
        cp -a "${STAGE_DIR}/ui/." /opt/evo/ui/
        chown -R "${SERVICE_USER}:${SERVICE_USER}" /opt/evo/ui
    fi

    # evo-ui-runtime binary + evo-ui.service unit. The runtime
    # is what the operator's browser hits on ports 80/443; the
    # framework's own :8443 wire surface is what the runtime
    # reverse-proxies to. Without the binary, `evo-ui.service`
    # fail-loops with `status=209/STDOUT` and the operator UI
    # is unreachable on the default ports — the state seen on
    # the fleet before this composition landed.
    if [[ -x "${STAGE_DIR}/ui-runtime/evo-ui-runtime" ]]; then
        install -m 0755 -o root -g root \
            "${STAGE_DIR}/ui-runtime/evo-ui-runtime" \
            /opt/evo/bin/evo-ui-runtime
    fi
    if [[ -f "${STAGE_DIR}/ui-runtime/evo-ui.service.in" ]]; then
        # Guarantee the StandardOutput directory exists BEFORE
        # the unit is enabled. evo-ui.service fails with
        # status=209/STDOUT when /opt/evo/ui/logs is absent;
        # enable+daemon-reload can race a start if the unit
        # was previously WantedBy=multi-user.target.
        install -d -m 0755 -o "${SERVICE_USER}" -g "${SERVICE_USER}" \
            /opt/evo/ui /opt/evo/ui/logs /opt/evo/ui/data /opt/evo/ui/releases
        local unit_tmp
        unit_tmp="$(mktemp)"
        sed -e "s|@SERVICE_USER@|${SERVICE_USER}|g" \
            "${STAGE_DIR}/ui-runtime/evo-ui.service.in" > "${unit_tmp}"
        if grep -qE '@[A-Z_]+@' "${unit_tmp}"; then
            echo "FAIL: evo-ui.service unit still carries unresolved @TOKEN@ placeholders" >&2
            rm -f "${unit_tmp}"
            exit 4
        fi
        install -m 0644 -o root -g root "${unit_tmp}" \
            /etc/systemd/system/evo-ui.service
        rm -f "${unit_tmp}"
        systemctl daemon-reload
        systemctl enable evo-ui.service >/dev/null 2>&1 || true
    fi
    # Sweep stale plugin bundles + install fresh.
    local d p p_name
    for d in /opt/evo/plugins/*/; do
        if [[ -d "$d" ]]; then
            rm -rf "$d"
        fi
    done
    for p in "${STAGE_DIR}/plugins/"*/; do
        [[ -d "$p" ]] || continue
        p_name="$(basename "$p")"
        [[ -f "${p}/manifest.toml" ]] || continue
        install -d -m 0755 -o root -g root "/opt/evo/plugins/${p_name}"
        install -m 0644 -o root -g root "${p}/manifest.toml" "/opt/evo/plugins/${p_name}/manifest.toml"
        install -m 0644 -o root -g root "${p}/manifest.sig" "/opt/evo/plugins/${p_name}/manifest.sig"
        install -m 0755 -o root -g root "${p}/plugin.bin" "/opt/evo/plugins/${p_name}/plugin.bin"
        # Privileges contract — runtime admission engine reads
        # this file at /opt/evo/plugins/<id>/privileges.yaml.
        # Every functional plugin bundle MUST ship it; the
        # bundle-build step enforces presence on the stage side,
        # so the file is always present here.
        install -m 0644 -o root -g root "${p}/privileges.yaml" "/opt/evo/plugins/${p_name}/privileges.yaml"
        # Per-plugin data files (e.g. DAC catalogue source).
        if [[ -d "${p}/data" ]]; then
            install -d -m 0755 -o root -g root "/opt/evo/plugins/${p_name}/data"
            cp -a "${p}/data/." "/opt/evo/plugins/${p_name}/data/"
        fi
    done
    # Catalogue: compose schema_version preamble + fragment.
    local catalogue_path="/opt/evo/catalogue/default.toml"
    local tmp_cat
    tmp_cat="$(mktemp)"
    {
        echo "# Composed by evo-install.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "# Source fragment: dist/catalogue/audio-rack.toml"
        echo ""
        echo "schema_version = 1"
        echo ""
        cat "${STAGE_DIR}/dist/catalogue/audio-rack.toml"
    } > "${tmp_cat}"
    install -m 0644 -o root -g root "${tmp_cat}" "${catalogue_path}"
    rm -f "${tmp_cat}"
}

install_main_systemd_unit() {
    # Main evo.service unit lives at /etc/systemd/system/evo.service.
    # bootstrap.sh handles only the .d/ drop-ins (exec-start.conf etc.);
    # the main unit is bundled with the steward and applied here so
    # bootstrap.sh's daemon-reload finds a complete unit + drop-in
    # pair. The framework reference unit (evo-core-eng) bakes
    # ExecStart=/opt/evo/bin/evo; the distribution drop-in
    # exec-start.conf clears that and substitutes
    # /opt/evo/bin/evo-device-audio.
    install -m 0644 -o root -g root \
        "${STAGE_DIR}/dist/systemd/evo.service" \
        /etc/systemd/system/evo.service
}

invoke_bootstrap_placement() {
    # Single canonical /etc placement primitive. bootstrap.sh
    # renders asound.conf with substituted card name, installs
    # sudoers drop-ins with the resolved service user, lays down
    # the .d/ drop-ins, plugins.d/ defaults (network probe_kind=off,
    # multiroom config), trust.d/ public keys, the operator ACL,
    # the MPD fragment + include injection, the modder directory,
    # the avahi disable, and runs its own verify section
    # (placeholder-residue check, amixer probe, aplay --dump-hw-params
    # probe). evo-install.sh's role is to FETCH + EXTRACT + INVOKE
    # bootstrap + add bundle-tier evidence on top — not to
    # reimplement placement in parallel.
    local bootstrap_path="${STAGE_DIR}/dist/scripts/bootstrap.sh"
    if [[ ! -f "${bootstrap_path}" ]]; then
        echo "FAIL: bundle missing dist/scripts/bootstrap.sh at ${bootstrap_path}" >&2
        exit 4
    fi
    local -a args=(--service-user "${SERVICE_USER}")
    if [[ -n "${EVO_INSTALL_AUDIO_CARD}" ]]; then
        args+=(--card "${EVO_INSTALL_AUDIO_CARD}")
    fi
    if [[ -n "${MULTIROOM_ROLE}" ]]; then
        args+=(--multiroom-role "${MULTIROOM_ROLE}")
    fi
    if [[ -n "${MULTIROOM_GROUP_ID}" ]]; then
        args+=(--multiroom-group-id "${MULTIROOM_GROUP_ID}")
    fi
    if [[ -n "${MULTIROOM_SOURCE_PCM}" ]]; then
        args+=(--multiroom-source-pcm "${MULTIROOM_SOURCE_PCM}")
    fi
    if [[ -n "${MULTIROOM_ALSA_PCM}" ]]; then
        args+=(--multiroom-alsa-pcm "${MULTIROOM_ALSA_PCM}")
    fi
    if [[ -n "${MULTIROOM_GROUP_MEMBERS}" ]]; then
        args+=(--multiroom-group-members "${MULTIROOM_GROUP_MEMBERS}")
    fi
    if [[ -n "${MULTIROOM_GROUP_MEMBER_ADDRESSES}" ]]; then
        args+=(--multiroom-group-member-addresses "${MULTIROOM_GROUP_MEMBER_ADDRESSES}")
    fi
    # EVO_DIST_DIR points bootstrap.sh at the bundle-staged tree
    # instead of its own script-relative dist/ parent. Subprocess
    # exit status propagates back via `set -e` — bootstrap.sh
    # exits 2 on placeholder-residue or visudo failure; the
    # install primitive surfaces that to the operator.
    EVO_DIST_DIR="${STAGE_DIR}/dist" bash "${bootstrap_path}" "${args[@]}"
}

purge_evo_mpd_includes() {
    # Strip ALL evo-related markers from /etc/mpd.conf so the
    # next inject lands on a clean baseline. Idempotent.
    # Targets:
    #   1. Lines `include "/etc/evo/mpd.conf"` (any whitespace).
    #   2. Lines `include_optional "/etc/evo/mpd.conf"` (any whitespace).
    #   3. Delimited block:
    #        # >>> evo-device-audio (...) — DO NOT EDIT >>>
    #        ...
    #        # <<< evo-device-audio (...) — DO NOT EDIT <<<
    #      Earlier bootstrap.sh and earlier evo-install.sh
    #      versions both wrap their canonical include in this
    #      block. Match by the `evo-device-audio` keyword in
    #      the start marker so future variants are caught.
    if [[ ! -f /etc/mpd.conf ]]; then
        return 0
    fi
    sed -i.pre-evo-purge \
        -e '/^[[:space:]]*include[[:space:]]\+"\/etc\/evo\/mpd\.conf"[[:space:]]*$/d' \
        -e '/^[[:space:]]*include_optional[[:space:]]\+"\/etc\/evo\/mpd\.conf"[[:space:]]*$/d' \
        -e '/^[[:space:]]*#[[:space:]]*>>>[[:space:]]*evo-device-audio.*>>>[[:space:]]*$/,/^[[:space:]]*#[[:space:]]*<<<[[:space:]]*evo-device-audio.*<<<[[:space:]]*$/d' \
        /etc/mpd.conf
    # Drop trailing blank lines left behind by the deletes.
    sed -i -e ':a' -e '/^$/{$d;N;ba' -e '}' /etc/mpd.conf
}

start_steward() {
    systemctl daemon-reload
    systemctl enable evo.service >/dev/null 2>&1 || true
    systemctl restart evo.service
}

# -------- Post-condition verification --------
ACTIVE_STATE=""
PLUGINS_ADMITTED=0
PLUGINS_EXPECTED=0
ADMISSION_FAILURES=0
NOT_DECLARED=0
CATALOGUE_SOURCE=""

JOURNAL_FAIL_HITS=""
JOURNAL_FAIL_COUNT=0
# Active PCM playback probe state. Set by verify_pcm_playback().
# Values: not_run / ok / busy / fail / skipped_no_aplay /
# skipped_no_probe_wav. Only `fail` participates in POST_OK
# gating; `busy` is evidence the chain works (MPD has the
# device).
PCM_PLAYBACK_PROBE="not_run"

# Count functional plugin bundles staged in the extracted
# bundle (directories under plugins/ that carry a
# manifest.toml). Dist-only template trees (sudoers wrappers
# without a wire binary) are excluded. This is the post-
# condition bar for "reinstall recreated the full operator
# surface" — a subset bundle must hard-fail, not greenwash.
count_expected_plugins_from_stage() {
    local p count=0
    [[ -d "${STAGE_DIR}/plugins" ]] || { echo 0; return; }
    for p in "${STAGE_DIR}/plugins/"*/; do
        [[ -f "${p}/manifest.toml" ]] || continue
        [[ -f "${p}/plugin.bin" ]] || continue
        count=$((count + 1))
    done
    echo "${count}"
}

verify_post_condition() {
    local deadline
    deadline=$(( $(date +%s) + 30 ))
    while [[ $(date +%s) -lt ${deadline} ]]; do
        if systemctl is-active evo >/dev/null 2>&1; then
            sleep 3   # let plugins admit
            break
        fi
        sleep 1
    done
    ACTIVE_STATE="$(systemctl is-active evo 2>/dev/null || echo unknown)"
    # Count admitted plugins by listing the per-plugin Unix
    # sockets the steward creates under /var/run/evo/plugins/.
    # Each successfully-admitted OOP plugin exposes its
    # request-socket here; this is observable substrate
    # independent of the steward's log-level filter (default
    # RUST_LOG=warn hides INFO-level "plugin admitted" lines).
    if [[ -d /var/run/evo/plugins ]]; then
        PLUGINS_ADMITTED=$(find /var/run/evo/plugins -maxdepth 1 -name '*.sock' 2>/dev/null | wc -l)
    else
        PLUGINS_ADMITTED=0
    fi
    PLUGINS_EXPECTED="$(count_expected_plugins_from_stage)"
    ADMISSION_FAILURES=$(journalctl -u evo --since "60 seconds ago" --no-pager -o cat 2>/dev/null | grep -c '^skipping plugin: admission failed$' || true)
    NOT_DECLARED=$(journalctl -u evo --since "60 seconds ago" --no-pager 2>/dev/null | grep -c 'not declared in the catalogue' || true)
    CATALOGUE_SOURCE=$(journalctl -u evo --since "60 seconds ago" --no-pager -o json 2>/dev/null | grep 'catalogue loaded' 2>/dev/null | grep -oE '"F_SOURCE":"[a-z]+"' 2>/dev/null | head -1 | sed 's/.*:"//; s/"$//' || true)

    # Strict: any line containing "fail" (case-insensitive)
    # in the evo journal, OR in the journal of any service
    # the install touched (mpd), is treated as install
    # failure. The operator's engineering bar: zero "fail"
    # across every consumer of the install's output.
    local fail_evo fail_mpd
    fail_evo=$(journalctl -u evo --since "60 seconds ago" --no-pager 2>/dev/null | grep -iE 'fail(ed|ure)?\b' || true)
    fail_mpd=$(journalctl -u mpd --since "60 seconds ago" --no-pager 2>/dev/null | grep -iE 'fail(ed|ure)?\b' || true)
    JOURNAL_FAIL_HITS="${fail_evo}"
    if [[ -n "${fail_mpd}" ]]; then
        JOURNAL_FAIL_HITS="${JOURNAL_FAIL_HITS}${JOURNAL_FAIL_HITS:+$'\n'}${fail_mpd}"
    fi
    if [[ -n "${JOURNAL_FAIL_HITS}" ]]; then
        JOURNAL_FAIL_COUNT=$(printf '%s\n' "${JOURNAL_FAIL_HITS}" | grep -c . || true)
    else
        JOURNAL_FAIL_COUNT=0
    fi

    verify_pcm_playback
    verify_smb_netbios_matches_hostname
    verify_lan_discovery_daemons_up
    verify_storage_usb_provisioning
}

# Storage-USB provisioning invariant: bootstrap Step 1g must
# have landed the wrapper at /usr/local/bin/evo-usb-mount
# (executable, mode 0755), the sudoers grant at
# /etc/sudoers.d/evo-storage-usb (mode 0440), the per-plugin
# state directory at /var/lib/evo/plugins/org.evoframework.
# storage.usb/, and every required binary on PATH (mount /
# umount / blockdev / fsck.vfat / fsck.exfat / ntfsfix /
# e2fsck / eject).
#
# The plugin's Rust runtime lands in Steps 2-6; those verify
# admission via the socket-count check already in the
# post-condition. THIS check covers what Step 1 owns: the
# provisioning surface the plugin will exec against.
#
# The `not-implemented` (exit 42) probe against the wrapper's
# `--version` action confirms the wrapper is invocable via
# sudo through the sudoers grant. The version string is
# stable across Steps 2-6 wrapper changes.
verify_storage_usb_provisioning() {
    STORAGE_USB_WRAPPER_OK="unknown"
    STORAGE_USB_SUDOERS_OK="unknown"
    STORAGE_USB_STATE_DIR_OK="unknown"
    STORAGE_USB_BINARIES_OK="unknown"

    # Wrapper — exists, mode 0755, --version returns exit 0 and
    # prints the stable version tag. Accepted versions:
    #   evo-usb-mount 2 — current (Step 4, mount takes 4 argv +
    #                     mount-opts allowlist; umount / umount-force
    #                     / eject actions actually execute).
    #   evo-usb-mount 1 — Step 1 stub (argv-validate only, all
    #                     actions exit 42). Accepted so a rolling
    #                     upgrade from a pre-Step-4 rig does not
    #                     hard-fail; the newer wrapper installs
    #                     over the older during the bootstrap
    #                     apply phase.
    if [[ -x /usr/local/bin/evo-usb-mount ]]; then
        local ver
        ver="$(/usr/local/bin/evo-usb-mount --version 2>/dev/null || true)"
        if [[ "${ver}" == "evo-usb-mount 2" ]] \
           || [[ "${ver}" == "evo-usb-mount 1" ]]; then
            STORAGE_USB_WRAPPER_OK="ok"
        else
            STORAGE_USB_WRAPPER_OK="wrong_version:${ver}"
        fi
    else
        STORAGE_USB_WRAPPER_OK="missing_or_not_executable"
    fi

    # Sudoers grant — file present + mode 0440 + owner root.
    if [[ -f /etc/sudoers.d/evo-storage-usb ]]; then
        local mode
        mode="$(stat -c '%a' /etc/sudoers.d/evo-storage-usb 2>/dev/null || echo unknown)"
        if [[ "${mode}" == "440" ]]; then
            STORAGE_USB_SUDOERS_OK="ok"
        else
            STORAGE_USB_SUDOERS_OK="wrong_mode:${mode}"
        fi
    else
        STORAGE_USB_SUDOERS_OK="missing"
    fi

    # Per-plugin state directory (aliases.toml lives here at
    # Step 6). Owner + mode enforcement happens at install
    # time; post-condition just checks presence + ownership.
    if [[ -d /var/lib/evo/plugins/org.evoframework.storage.usb/state ]]; then
        STORAGE_USB_STATE_DIR_OK="ok"
    else
        STORAGE_USB_STATE_DIR_OK="missing"
    fi

    # Binary union — every tool the plugin's wrapper actions
    # will exec (per USB-STORAGE.md §2 matrix + §9 safe-remove).
    local missing_bins=()
    for bin in mount umount blockdev fsck.vfat fsck.exfat ntfsfix e2fsck eject lsblk findmnt; do
        if ! command -v "${bin}" >/dev/null 2>&1; then
            missing_bins+=("${bin}")
        fi
    done
    if [[ ${#missing_bins[@]} -eq 0 ]]; then
        STORAGE_USB_BINARIES_OK="ok"
    else
        STORAGE_USB_BINARIES_OK="missing:${missing_bins[*]}"
    fi

    if [[ "${STORAGE_USB_WRAPPER_OK}" == "ok" \
       && "${STORAGE_USB_SUDOERS_OK}" == "ok" \
       && "${STORAGE_USB_STATE_DIR_OK}" == "ok" \
       && "${STORAGE_USB_BINARIES_OK}" == "ok" ]]; then
        STORAGE_USB_PROVISIONING_CHECK="ok"
    else
        STORAGE_USB_PROVISIONING_CHECK="degraded"
        echo "" >&2
        echo "FAIL: storage.usb provisioning incomplete." >&2
        echo "      wrapper : ${STORAGE_USB_WRAPPER_OK}" >&2
        echo "      sudoers : ${STORAGE_USB_SUDOERS_OK}" >&2
        echo "      state   : ${STORAGE_USB_STATE_DIR_OK}" >&2
        echo "      binaries: ${STORAGE_USB_BINARIES_OK}" >&2
        echo "      Re-run bootstrap without EVO_INSTALL_STORAGE_USB=0, or" >&2
        echo "      apt-install the missing binaries manually." >&2
    fi
}

# LAN-discovery invariant: `avahi-daemon` publishes the SMB
# service via mDNS (`_smb._tcp`); `nmbd` publishes the NetBIOS
# name. Both are required for the device to appear in Ubuntu /
# Finder / Windows network browsers under its hostname. An
# install that leaves either one inactive silently degrades LAN
# visibility — the class the operator hit fleet-wide when an
# earlier bootstrap step disabled avahi.
verify_lan_discovery_daemons_up() {
    local avahi nmbd
    avahi="$(systemctl is-active avahi-daemon.service 2>/dev/null || echo unknown)"
    nmbd="$(systemctl is-active nmbd.service 2>/dev/null || echo unknown)"
    LAN_DISCOVERY_AVAHI="${avahi}"
    LAN_DISCOVERY_NMBD="${nmbd}"
    if [[ "${avahi}" == "active" && "${nmbd}" == "active" ]]; then
        LAN_DISCOVERY_CHECK="ok"
    else
        LAN_DISCOVERY_CHECK="degraded"
        echo "" >&2
        echo "FAIL: LAN discovery degraded." >&2
        [[ "${avahi}" != "active" ]] && \
            echo "      avahi-daemon.service is '${avahi}' — SMB will not appear via mDNS." >&2
        [[ "${nmbd}" != "active" ]] && \
            echo "      nmbd.service is '${nmbd}' — SMB will not appear via NetBIOS." >&2
        echo "      Fix: sudo systemctl enable --now avahi-daemon.service nmbd.service" >&2
    fi
}

# LAN-identity invariant: the SMB server's `netbios name` in
# `/etc/samba/smb.conf` MUST equal the OS hostname read at
# post-condition time. This closes the class that caused the
# fleet-wide `netbios name = EvoDevice` collision: the plugin
# is now supposed to derive the name from
# `/proc/sys/kernel/hostname` at every apply, and this
# assertion verifies the rendered conf carries the correct
# value. If the plugin has been admitted but the file still
# names the last-resort default (or any other mismatch), we
# refuse the install.
#
# smb-server may be disabled by policy — in that case there is
# no `netbios name` line to check and we skip.
verify_smb_netbios_matches_hostname() {
    local conf="/etc/samba/smb.conf"
    if [[ ! -r "${conf}" ]]; then
        SMB_NETBIOS_CHECK="skipped_no_smb_conf"
        return 0
    fi
    local live_hostname netbios_in_conf
    live_hostname="$(cat /proc/sys/kernel/hostname 2>/dev/null || true)"
    netbios_in_conf="$(awk -F'=' '
        /^\[/ { in_global = ($0 == "[global]"); next }
        in_global && $1 ~ /^[[:space:]]*netbios[[:space:]]+name[[:space:]]*$/ {
            gsub(/^[[:space:]]+|[[:space:]]+$/, "", $2)
            print $2
            exit
        }
    ' "${conf}")"
    if [[ -z "${netbios_in_conf}" ]]; then
        SMB_NETBIOS_CHECK="skipped_no_netbios_line"
        return 0
    fi
    if [[ "${netbios_in_conf}" == "${live_hostname}" ]]; then
        SMB_NETBIOS_CHECK="ok"
    else
        SMB_NETBIOS_CHECK="mismatch"
        echo "" >&2
        echo "FAIL: /etc/samba/smb.conf carries 'netbios name = ${netbios_in_conf}'" >&2
        echo "      but /proc/sys/kernel/hostname is '${live_hostname}'." >&2
        echo "      The LAN-identity invariant requires them to match." >&2
        echo "      Trigger a re-apply of the smb-server plugin so the" >&2
        echo "      renderer picks up the live hostname:" >&2
        echo "        evo-plugin-tool ... network.smb_server.apply ..." >&2
        echo "      Or run: sudo systemctl restart evo && sleep 5 && re-check." >&2
    fi
}

# Active PCM playback-path probe at post-condition time. The
# bootstrap-tier probe runs against pcm.evo before the steward
# starts; this one runs AFTER the steward + plugin admission +
# delivery.alsa's drop-in rewrite (if any), so it observes the
# operator-visible final state. EBUSY from MPD holding the PCM
# is reported as `busy` — that proves the chain opens; only
# open-failure (`No such device`, `Cannot get card index`, etc.)
# is the regression class the gate must catch.
verify_pcm_playback() {
    local probe_wav=""
    if [[ -f "${STAGE_DIR}/dist/alsa/silent-probe.wav" ]]; then
        probe_wav="${STAGE_DIR}/dist/alsa/silent-probe.wav"
    elif [[ -f /usr/share/sounds/alsa/Front_Center.wav ]]; then
        probe_wav="/usr/share/sounds/alsa/Front_Center.wav"
    fi
    if ! command -v aplay >/dev/null 2>&1; then
        PCM_PLAYBACK_PROBE="skipped_no_aplay"
        return 0
    fi
    if [[ -z "${probe_wav}" ]]; then
        PCM_PLAYBACK_PROBE="skipped_no_probe_wav"
        return 0
    fi
    local probe_out probe_exit
    set +e
    probe_out="$(aplay -D evo --dump-hw-params "${probe_wav}" 2>&1)"
    probe_exit=$?
    set -e
    if [[ ${probe_exit} -eq 0 ]] \
        && printf '%s' "${probe_out}" | grep -q '^HW Params of device "evo":'; then
        PCM_PLAYBACK_PROBE="ok"
    elif printf '%s' "${probe_out}" | grep -qiE 'device or resource busy|EBUSY'; then
        PCM_PLAYBACK_PROBE="busy"
    else
        PCM_PLAYBACK_PROBE="fail"
        echo "FAIL: pcm.evo playback probe (aplay --dump-hw-params -D evo) failed:" >&2
        printf '%s\n' "${probe_out}" | head -5 | sed 's/^/  /' >&2
    fi
}

# -------- Music-library hash verification --------
MUSIC_HASH_PRE=""
MUSIC_HASH_POST=""
MUSIC_HASH_PRESERVED="true"
MUSIC_HASH_CHANGED="false"

verify_music_hashes_preserved() {
    MUSIC_HASH_POST="$(snapshot_music_hashes)"
    if [[ "${MUSIC_HASH_PRE}" == "${MUSIC_HASH_POST}" ]]; then
        MUSIC_HASH_PRESERVED="true"
    else
        MUSIC_HASH_PRESERVED="false"
        echo "FAIL: music library hash diverged" >&2
        echo "      pre  = ${MUSIC_HASH_PRE}" >&2
        echo "      post = ${MUSIC_HASH_POST}" >&2
    fi
}

# -------- Evidence emission --------
emit_evidence() {
    local out_path="${EVO_INSTALL_EVIDENCE_OUT}"
    if [[ -z "${out_path}" ]]; then
        install -d -m 0755 -o root -g root /var/lib/evo/evidence
        out_path="/var/lib/evo/evidence/${PRIMITIVE_ID}-${ARCH}.toml"
    else
        install -d -m 0755 -o root -g root "$(dirname "${out_path}")"
    fi

    local end_ns end_utc elapsed_ms
    end_ns="$(date -u +%s%N)"
    end_utc="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    elapsed_ms=$(( (end_ns - START_NS) / 1000000 ))

    local service_active_bool="false"
    if [[ "${ACTIVE_STATE}" == "active" ]]; then
        service_active_bool="true"
    fi

    local music_hash_pre_field="\"\""
    if [[ -n "${MUSIC_HASH_PRE}" ]]; then
        music_hash_pre_field="\"${MUSIC_HASH_PRE}\""
    fi
    local music_hash_post_field="\"\""
    if [[ -n "${MUSIC_HASH_POST}" ]]; then
        music_hash_post_field="\"${MUSIC_HASH_POST}\""
    fi

    # Compose the unsigned evidence record. Signing happens
    # below if EVO_ACCEPTANCE_SIGNING_KEY is set.
    local body_path="${WORK_DIR}/evidence-body.toml"
    cat > "${body_path}" <<EOF
schema_version = 1
primitive = "${PRIMITIVE_ID}"
architecture = "${ARCH}"
ran_at_utc = "${end_utc}"
started_at_utc = "${START_UTC}"
elapsed_ms = ${elapsed_ms}
mode = "${MODE}"
bundle_url = "${BUNDLE_URL}"
bundle_sha256 = "${BUNDLE_SHA256:-}"
bundle_size_bytes = ${BUNDLE_SIZE}

[post_condition]
service_active = ${service_active_bool}
plugins_admitted_count = ${PLUGINS_ADMITTED}
plugins_expected_count = ${PLUGINS_EXPECTED}
admission_failures = ${ADMISSION_FAILURES}
subject_not_declared = ${NOT_DECLARED}
catalogue_source = "${CATALOGUE_SOURCE:-unknown}"
music_library_hash_pre = ${music_hash_pre_field}
music_library_hash_post = ${music_hash_post_field}
music_library_hash_preserved = ${MUSIC_HASH_PRESERVED}
music_library_hash_changed = ${MUSIC_HASH_CHANGED}
pcm_playback_probe = "${PCM_PLAYBACK_PROBE}"
smb_netbios_check = "${SMB_NETBIOS_CHECK:-unknown}"
lan_discovery_check = "${LAN_DISCOVERY_CHECK:-unknown}"
lan_discovery_avahi = "${LAN_DISCOVERY_AVAHI:-unknown}"
lan_discovery_nmbd = "${LAN_DISCOVERY_NMBD:-unknown}"
storage_usb_provisioning_check = "${STORAGE_USB_PROVISIONING_CHECK:-unknown}"
storage_usb_wrapper = "${STORAGE_USB_WRAPPER_OK:-unknown}"
storage_usb_sudoers = "${STORAGE_USB_SUDOERS_OK:-unknown}"
storage_usb_state_dir = "${STORAGE_USB_STATE_DIR_OK:-unknown}"
storage_usb_binaries = "${STORAGE_USB_BINARIES_OK:-unknown}"

EOF

    # Signature block. Signed when EVO_ACCEPTANCE_SIGNING_KEY
    # is set; otherwise a placeholder records the run as
    # unsigned (the validation harness re-runs with a key for
    # the release-cut preflight).
    if [[ -n "${EVO_ACCEPTANCE_SIGNING_KEY}" && -r "${EVO_ACCEPTANCE_SIGNING_KEY}" ]]; then
        local sig_bin sig_b64
        sig_bin="${WORK_DIR}/evidence.sig"
        if openssl pkeyutl -sign \
                -inkey "${EVO_ACCEPTANCE_SIGNING_KEY}" \
                -rawin -in "${body_path}" \
                -out "${sig_bin}" >/dev/null 2>&1; then
            sig_b64="$(base64 -w0 < "${sig_bin}")"
            cat > "${out_path}" <<EOF
$(cat "${body_path}")
[signature]
key_id = "evo-acceptance-signing"
ed25519_b64 = "${sig_b64}"
EOF
        else
            echo "  WARN: evidence signing failed; writing unsigned record" >&2
            cat > "${out_path}" <<EOF
$(cat "${body_path}")
[signature]
key_id = "evo-acceptance-signing"
ed25519_b64 = "UNSIGNED_SIGNING_ERROR"
EOF
        fi
    else
        cat > "${out_path}" <<EOF
$(cat "${body_path}")
[signature]
key_id = "evo-acceptance-signing"
ed25519_b64 = "UNSIGNED_OPERATOR_RUN"
EOF
    fi
    chmod 0644 "${out_path}"
    echo "Evidence: ${out_path}"
}

# -------- Mode dispatch --------
init_work_dir

case "${MODE}" in
    install)
        echo "[1/7] fetch bundle ..."     ; fetch_and_verify_bundle ; echo "  ok (sha256: ${BUNDLE_SHA256})"
        echo "[2/7] extract bundle ..."   ; extract_bundle          ; echo "  ok"
        echo "[3/7] system packages (baseline + per-plugin prerequisites, parity-verified) ..." ; ensure_system_packages ; echo "  ok"
        echo "[4/7] stop prior steward ..." ; stop_prior_steward    ; echo "  ok"
        echo "[5/7] /opt/evo (binaries + plugins + catalogue) ..." ; place_opt_evo  ; echo "  ok"
        echo "[6/7] /etc/evo + sudoers + drop-ins + trust roots + music-library boilerplate ..." ; install_main_systemd_unit ; invoke_bootstrap_placement ; echo "  ok"
        echo "[7/7] start + verify ..."   ; start_steward ; verify_post_condition
        ;;
    reinstall)
        echo "[1/8] fetch bundle ..."    ; fetch_and_verify_bundle ; echo "  ok (sha256: ${BUNDLE_SHA256})"
        echo "[2/8] extract bundle ..."  ; extract_bundle          ; echo "  ok"
        echo "[3/8] system packages (baseline + per-plugin prerequisites, parity-verified) ..." ; ensure_system_packages ; echo "  ok"
        echo "[4/8] FULL WIPE (binaries + config + state + music) ..."
        wipe_full ; echo "  ok"
        echo "[5/8] /opt/evo ..."        ; place_opt_evo           ; echo "  ok"
        echo "[6/8] /etc/evo + sudoers + drop-ins + trust roots + music-library boilerplate ..." ; install_main_systemd_unit ; invoke_bootstrap_placement ; echo "  ok"
        echo "[7/8] start + verify ..."  ; start_steward ; verify_post_condition
        MUSIC_HASH_CHANGED="true"
        ;;
    wipe-config)
        echo "[1/8] snapshot music library hashes ..." ; MUSIC_HASH_PRE="$(snapshot_music_hashes)" ; echo "  ok (sha256: ${MUSIC_HASH_PRE})"
        echo "[2/8] fetch bundle ..."    ; fetch_and_verify_bundle ; echo "  ok (sha256: ${BUNDLE_SHA256})"
        echo "[3/8] extract bundle ..."  ; extract_bundle          ; echo "  ok"
        echo "[4/8] system packages (baseline + per-plugin prerequisites, parity-verified) ..." ; ensure_system_packages ; echo "  ok"
        echo "[5/8] CONFIG WIPE (binaries + config + state, music preserved) ..."
        wipe_config ; echo "  ok"
        echo "[6/8] /opt/evo ..."        ; place_opt_evo           ; echo "  ok"
        echo "[7/8] /etc/evo + sudoers + drop-ins + trust roots + music-library boilerplate ..." ; install_main_systemd_unit ; invoke_bootstrap_placement ; echo "  ok"
        echo "[8/8] start + verify + music library byte-equal ..."  ; start_steward ; verify_post_condition ; verify_music_hashes_preserved
        ;;
    wipe-user-data)
        echo "[1/7] snapshot music library hashes ..." ; MUSIC_HASH_PRE="$(snapshot_music_hashes)" ; echo "  ok (sha256: ${MUSIC_HASH_PRE})"
        echo "[2/7] fetch bundle (for /etc/evo baseline) ..." ; fetch_and_verify_bundle ; echo "  ok (sha256: ${BUNDLE_SHA256})"
        echo "[3/7] extract bundle ..."   ; extract_bundle          ; echo "  ok"
        echo "[4/7] USER-DATA VACUUM (operator-generated state, /etc/evo overrides reset; binaries + music preserved) ..."
        wipe_user_data ; echo "  ok"
        echo "[5/7] /etc/evo baseline (re-apply) + drop-ins + sudoers + music-library boilerplate ..." ; install_main_systemd_unit ; invoke_bootstrap_placement ; echo "  ok"
        echo "[6/7] start + verify ..."   ; start_steward ; verify_post_condition
        echo "[7/7] verify music library byte-equal ..." ; verify_music_hashes_preserved
        ;;
esac

echo ""
echo "  service:               ${ACTIVE_STATE}"
echo "  plugins admitted:      ${PLUGINS_ADMITTED} (expected ${PLUGINS_EXPECTED})"
echo "  admission failures:    ${ADMISSION_FAILURES}"
echo "  not-declared warnings: ${NOT_DECLARED}"
echo "  catalogue source:      ${CATALOGUE_SOURCE:-unknown}"
echo "  journal fail hits:     ${JOURNAL_FAIL_COUNT}"
echo "  pcm.evo playback:      ${PCM_PLAYBACK_PROBE}"
if [[ "${MODE}" == "wipe-config" || "${MODE}" == "wipe-user-data" ]]; then
    echo "  music library hash:    ${MUSIC_HASH_PRESERVED} (pre=${MUSIC_HASH_PRE} post=${MUSIC_HASH_POST})"
fi
if [[ "${JOURNAL_FAIL_COUNT}" -gt 0 ]]; then
    echo ""
    echo "  journal fail lines:"
    printf '%s\n' "${JOURNAL_FAIL_HITS}" | sed 's/^/    /'
fi
echo ""

POST_OK=1
if [[ "${ACTIVE_STATE}" != "active" ]]; then POST_OK=0; fi
if [[ "${PLUGINS_ADMITTED}" -lt 1 ]]; then POST_OK=0; fi
# Bundle-declared plugin set must fully admit. A green
# "9 of 18" post-condition is how --mode=reinstall previously
# wiped shares/smb-server/notifications and left UI shelves
# dead while still exiting 0.
if [[ "${PLUGINS_EXPECTED}" -lt 1 ]]; then POST_OK=0; fi
if [[ "${PLUGINS_ADMITTED}" -lt "${PLUGINS_EXPECTED}" ]]; then POST_OK=0; fi
if [[ "${ADMISSION_FAILURES}" -ne 0 ]]; then POST_OK=0; fi
if [[ "${NOT_DECLARED}" -ne 0 ]]; then POST_OK=0; fi
if [[ "${JOURNAL_FAIL_COUNT}" -gt 0 ]]; then POST_OK=0; fi
# The PCM playback-path probe is the dedicated catch for the
# regression class that the old gate missed: a placement that
# leaves pcm.evo unopenable for playback while the steward +
# plugin admission look healthy. `fail` is the only state that
# breaks the gate; `busy` is positive evidence (MPD has the
# device); the `skipped_*` states are documented gaps the
# evidence record carries forward.
if [[ "${PCM_PLAYBACK_PROBE}" == "fail" ]]; then POST_OK=0; fi
# LAN-identity invariant. `mismatch` is a wire-visible defect
# (the fleet would collide on `netbios name = EvoDevice` or on
# any other stale value). The `skipped_*` states name a
# structural absence (no smb.conf yet, or the plugin has been
# disabled by policy) and are not a failure.
if [[ "${SMB_NETBIOS_CHECK:-unknown}" == "mismatch" ]]; then POST_OK=0; fi
# LAN-discovery invariant. `degraded` means either avahi or
# nmbd (or both) is inactive at install-complete — the class
# that silently hides the device from Ubuntu / Finder / Windows
# network browsers. Refuse the install so the bootstrap-tier
# step 2.7 has to have activated both.
if [[ "${LAN_DISCOVERY_CHECK:-unknown}" == "degraded" ]]; then POST_OK=0; fi
# Storage-USB provisioning invariant. `degraded` means the
# bootstrap-tier Step 1g did not land one or more of: the
# wrapper at /usr/local/bin/evo-usb-mount, the sudoers grant at
# /etc/sudoers.d/evo-storage-usb, the per-plugin state dir, or
# the union of FS-repair binaries. The plugin's mount / repair
# / eject verbs will fail at runtime without these — refuse
# the install so the deploy cannot silently declare success on
# a rig where the block-storage privilege path is broken.
if [[ "${STORAGE_USB_PROVISIONING_CHECK:-unknown}" == "degraded" ]]; then POST_OK=0; fi
if [[ "${MODE}" == "wipe-config" || "${MODE}" == "wipe-user-data" ]]; then
    if [[ "${MUSIC_HASH_PRESERVED}" != "true" ]]; then POST_OK=0; fi
fi

emit_evidence

if [[ "${POST_OK}" -eq 1 ]]; then
    echo "=== evo-install.sh ${MODE} complete ==="
    echo "Service active. ${PLUGINS_ADMITTED} plugins admitted."
    echo ""
    echo "Next steps:"
    echo "  - Inspect: systemctl status evo"
    echo "  - Operator wizard: evo-plugin-tool admin device identity show"
    if [[ "${MODE}" != "wipe-user-data" ]]; then
        echo "  - Music library: /var/lib/evo/music/{INTERNAL,USB,NAS}"
    fi
    exit 0
else
    echo "FAIL: post-condition verification failed" >&2
    echo "      Check: journalctl -u evo --no-pager -n 80" >&2
    exit 5
fi
