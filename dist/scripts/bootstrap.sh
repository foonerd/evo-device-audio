#!/usr/bin/env bash
# bootstrap.sh — install evo-device-audio reference distribution
# artefacts on the target host.
#
# This script is the distribution-tier dual of the framework's
# Privilege Preflight Admission Gate (PPAG): the gate verifies
# that the runtime preconditions for each declared
# CapabilityIntent are satisfied; this script CREATES those
# preconditions. Operators run it once after installing the
# steward binary; the plugin's admission-time preflight then
# confirms the install was successful.
#
# Idempotent: every step checks current state before writing.
# Re-running on an already-bootstrapped host is a no-op (but
# the verify line at the end re-confirms the install).
#
# Operator-readable: every action prints a single line so the
# bring-up log captures what changed.
#
# Usage:
#   sudo dist/scripts/bootstrap.sh                # all steps
#   sudo dist/scripts/bootstrap.sh --skip-systemd # skip the
#                                                 # systemd
#                                                 # drop-ins
#   sudo dist/scripts/bootstrap.sh --service-user evo
#                                                 # explicit
#                                                 # user
#
# Exit codes:
#   0 — bootstrap completed; PPAG-side verification succeeded.
#   1 — operator error (wrong invocation, missing prerequisite).
#   2 — install error (a step failed; previous steps left in place).
#
# Toggles:
#   EVO_INSTALL_MPD_SUDOERS=0          — skip /etc/sudoers.d/evo-mpd-restart
#   EVO_INSTALL_NETWORK_NM_SUDOERS=0   — skip /etc/sudoers.d/evo-network-nm
#   EVO_INSTALL_HARDWARE_AUDIO_SUDOERS=0 — skip /etc/sudoers.d/evo-hardware-audio
#   EVO_INSTALL_SYSTEM_POWER_SUDOERS=0  — skip /etc/sudoers.d/evo-system-power
#   EVO_INSTALL_DACS_CATALOGUE=0       — skip /usr/share/evo-device-audio/dacs.json install
#   EVO_INSTALL_SYSTEMD_DROP_INS=0     — skip /etc/systemd/system/evo.service.d/
#   EVO_INSTALL_CLIENT_ACL=0           — skip /etc/evo/client_acl.toml install
#   EVO_INSTALL_MPD_FRAGMENT=0         — skip /etc/evo/mpd.conf bootstrap (empty file)
#   EVO_INSTALL_ASOUND_CONF=0          — skip /etc/asound.conf install
#   EVO_INSTALL_CATALOGUE=0            — skip /opt/evo/catalogue/default.toml install
#   EVO_INSTALL_MPD_INCLUDE=0          — skip injecting include of /etc/evo/mpd.conf
#                                       into /etc/mpd.conf
#   EVO_AUDIO_CARD=<name>              — override auto-detected ALSA card name
#                                       (env-var form; also available as --card)
#   EVO_PRESERVE_MULTIROOM_TOML=1      — skip overwriting an existing
#                                       /etc/evo/plugins.d/org.evoframework.multiroom.evo-native.toml.
#                                       Default (unset or 0) overwrites from the
#                                       bootstrap flags every run, matching the
#                                       fresh-install ergonomic. Set to 1 in
#                                       binary-upgrade flows to preserve operator-
#                                       tuned member lists across bootstrap re-runs.
#
# Per-step toggles let operators disable individual install
# legs without editing this script — useful when a vendor
# distribution composes its own privileged-action surface
# alongside the reference one.

set -euo pipefail

# Resolve the script's own directory so dist/* paths resolve
# regardless of the operator's CWD.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# DIST_DIR defaults to the script's parent (canonical layout
# `dist/scripts/bootstrap.sh` → `dist/`). Operator-facing
# installers that stage the dist tree elsewhere (e.g.
# evo-install.sh extracts a signed bundle to a temp directory)
# override via EVO_DIST_DIR. The override lets a single
# canonical placement primitive serve both on-target operators
# and the bundle-driven flow without parallel implementations.
DIST_DIR="${EVO_DIST_DIR:-$(cd "$SCRIPT_DIR/.." && pwd)}"

# Defaults.
SERVICE_USER=""
SYSTEMCTL_BIN="/usr/bin/systemctl"
SUDOERS_FILE="/etc/sudoers.d/evo-mpd-restart"
NETWORK_NM_SUDOERS_FILE="/etc/sudoers.d/evo-network-nm"
HARDWARE_AUDIO_SUDOERS_FILE="/etc/sudoers.d/evo-hardware-audio"
SYSTEM_POWER_SUDOERS_FILE="/etc/sudoers.d/evo-system-power"
DACS_CATALOGUE_DIR="/usr/share/evo-device-audio"
DACS_CATALOGUE_PATH="${DACS_CATALOGUE_DIR}/dacs.json"
NMCLI_BIN="/usr/bin/nmcli"
SYSTEMD_DROPIN_DIR="/etc/systemd/system/evo.service.d"
MPD_FRAGMENT_PATH="/etc/evo/mpd.conf"
MPD_CONF_PATH="/etc/mpd.conf"
ASOUND_CONF_PATH="/etc/asound.conf"
PLUGINS_D_DIR="/etc/evo/plugins.d"
SKIP_SYSTEMD=0
AUDIO_CARD="${EVO_AUDIO_CARD:-}"
MULTIROOM_ROLE=""
MULTIROOM_GROUP_ID=""
MULTIROOM_SOURCE_PCM=""
MULTIROOM_ALSA_PCM=""
MULTIROOM_GROUP_MEMBERS=""
MULTIROOM_GROUP_MEMBER_ADDRESSES=""

# Argument parsing — minimal; positional args not supported.
while [[ $# -gt 0 ]]; do
    case "$1" in
        --service-user)
            SERVICE_USER="$2"
            shift 2
            ;;
        --service-user=*)
            SERVICE_USER="${1#--service-user=}"
            shift
            ;;
        --card)
            AUDIO_CARD="$2"
            shift 2
            ;;
        --card=*)
            AUDIO_CARD="${1#--card=}"
            shift
            ;;
        --skip-systemd)
            SKIP_SYSTEMD=1
            shift
            ;;
        --multiroom-role)
            MULTIROOM_ROLE="$2"
            shift 2
            ;;
        --multiroom-role=*)
            MULTIROOM_ROLE="${1#--multiroom-role=}"
            shift
            ;;
        --multiroom-group-id)
            MULTIROOM_GROUP_ID="$2"
            shift 2
            ;;
        --multiroom-group-id=*)
            MULTIROOM_GROUP_ID="${1#--multiroom-group-id=}"
            shift
            ;;
        --multiroom-source-pcm)
            MULTIROOM_SOURCE_PCM="$2"
            shift 2
            ;;
        --multiroom-source-pcm=*)
            MULTIROOM_SOURCE_PCM="${1#--multiroom-source-pcm=}"
            shift
            ;;
        --multiroom-alsa-pcm)
            MULTIROOM_ALSA_PCM="$2"
            shift 2
            ;;
        --multiroom-alsa-pcm=*)
            MULTIROOM_ALSA_PCM="${1#--multiroom-alsa-pcm=}"
            shift
            ;;
        --multiroom-group-members)
            MULTIROOM_GROUP_MEMBERS="$2"
            shift 2
            ;;
        --multiroom-group-members=*)
            MULTIROOM_GROUP_MEMBERS="${1#--multiroom-group-members=}"
            shift
            ;;
        --multiroom-group-member-addresses)
            MULTIROOM_GROUP_MEMBER_ADDRESSES="$2"
            shift 2
            ;;
        --multiroom-group-member-addresses=*)
            MULTIROOM_GROUP_MEMBER_ADDRESSES="${1#--multiroom-group-member-addresses=}"
            shift
            ;;
        -h|--help)
            grep -E '^# ' "$0" | sed 's/^# //'
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            echo "usage: $0 [--service-user <name>] [--card <NAME>] [--skip-systemd] \\" >&2
            echo "    [--multiroom-role source|receiver|none] [--multiroom-group-id <uuid>] \\" >&2
            echo "    [--multiroom-source-pcm <alsa-pcm>] [--multiroom-alsa-pcm <alsa-pcm>] \\" >&2
            echo "    [--multiroom-group-members \"<dev-id>,<dev-id>,...\"] \\" >&2
            echo "    [--multiroom-group-member-addresses \"<host>:<port>,<host>:<port>,...\"]" >&2
            exit 1
            ;;
    esac
done

# Authority check: this script runs as root because it writes
# under /etc and chowns paths. Refuse loudly if not root.
if [[ $EUID -ne 0 ]]; then
    echo "bootstrap.sh must run as root (writes /etc/sudoers.d, /etc/systemd, /etc/evo)" >&2
    exit 1
fi

# Resolve the steward service user. Operator override wins;
# otherwise pick the appliance-class default (operator's
# first user at uid >= 1000), matching the convention in
# the framework's PLUGIN_PACKAGING.md.
if [[ -z "$SERVICE_USER" ]]; then
    SERVICE_USER="$(getent passwd | awk -F: '$3 >= 1000 && $3 < 65534 { print $1; exit }')"
    if [[ -z "$SERVICE_USER" ]]; then
        echo "could not resolve service user (no uid >= 1000 found); pass --service-user <name>" >&2
        exit 1
    fi
fi
echo "[bootstrap] service user: $SERVICE_USER"

# Verify the user exists.
if ! id -u "$SERVICE_USER" >/dev/null 2>&1; then
    echo "service user $SERVICE_USER does not exist" >&2
    exit 1
fi

# Resolve the systemctl binary.
if [[ ! -x "$SYSTEMCTL_BIN" ]]; then
    # Fall back to PATH lookup so distributions on non-
    # standard prefixes (Alpine /sbin/systemctl) still
    # bootstrap.
    SYSTEMCTL_BIN="$(command -v systemctl || true)"
    if [[ -z "$SYSTEMCTL_BIN" ]]; then
        echo "systemctl not found on PATH; this script needs systemd" >&2
        exit 1
    fi
fi
echo "[bootstrap] systemctl binary: $SYSTEMCTL_BIN"

# ----------------------------------------------------------
# Resolve the ALSA card name the modular pipeline targets.
# Operator override wins (env var EVO_AUDIO_CARD or --card
# flag); otherwise pick the first playback card reported by
# `aplay -l` excluding the filter classes documented in
# detect_audio_card_from_aplay_output. Refuse the install
# with an operator-readable error when no playback card is
# available (e.g. headless container, audio kernel modules
# absent). Reference distribution uses the I-Sabre Q2M card
# (name `DAC`); every other deployment substitutes its
# detected card.
# ----------------------------------------------------------

# detect_audio_card_from_aplay_output: pure parser sourced
# from lib/detect-audio-card.sh. Kept as a separate file so
# the regression test suite can drive it with synthetic
# `aplay -l` fixtures without executing the rest of this
# script.
# shellcheck source=lib/detect-audio-card.sh
. "$SCRIPT_DIR/lib/detect-audio-card.sh"

if [[ -z "$AUDIO_CARD" ]]; then
    if ! command -v aplay >/dev/null 2>&1; then
        echo "aplay not found on PATH; install alsa-utils or pass --card <NAME>" >&2
        exit 1
    fi
    if ! AUDIO_CARD="$(aplay -l 2>/dev/null | detect_audio_card_from_aplay_output)"; then
        echo "no ALSA playback card detected via aplay -l; pass --card <NAME> to override" >&2
        echo "  (current aplay -l output:)" >&2
        aplay -l 2>&1 | sed 's/^/  /' >&2
        exit 2
    fi
    echo "[bootstrap] detected ALSA playback card: $AUDIO_CARD"
else
    echo "[bootstrap] ALSA playback card (operator override): $AUDIO_CARD"
fi

# ----------------------------------------------------------
# Step 1: /etc/sudoers.d/evo-mpd-restart (narrow NOPASSWD)
# ----------------------------------------------------------
if [[ "${EVO_INSTALL_MPD_SUDOERS:-1}" != "0" ]]; then
    TEMPLATE="$DIST_DIR/sudoers.d/evo-mpd-restart.in"
    if [[ ! -f "$TEMPLATE" ]]; then
        echo "sudoers template not found at $TEMPLATE" >&2
        exit 2
    fi
    TMP="$(mktemp)"
    trap 'rm -f "$TMP"' EXIT
    sed -e "s|@EVO_SERVICE_USER@|$SERVICE_USER|g" \
        -e "s|/usr/bin/systemctl|$SYSTEMCTL_BIN|g" \
        "$TEMPLATE" > "$TMP"

    # visudo -c verifies syntax before we install. A
    # malformed sudoers drop-in can lock the operator out
    # of sudo entirely; the check prevents that.
    if ! visudo -c -f "$TMP" >/dev/null; then
        echo "rendered sudoers fragment failed visudo -c; refusing to install" >&2
        echo "  rendered file kept at $TMP for inspection" >&2
        trap - EXIT
        exit 2
    fi

    install -m 0440 -o root -g root "$TMP" "$SUDOERS_FILE"
    rm -f "$TMP"
    trap - EXIT
    echo "[bootstrap] installed $SUDOERS_FILE"
else
    echo "[bootstrap] EVO_INSTALL_MPD_SUDOERS=0 — skipping sudoers drop-in"
fi

# ----------------------------------------------------------
# Step 1b: /etc/sudoers.d/evo-network-nm (narrow NOPASSWD)
# ----------------------------------------------------------
# Mirrors Step 1 for the network.nm plugin's nmcli surface.
# Both PPAG consumers in this distribution share the same
# sudoers-drop-in install discipline: render the template
# with the resolved service user + binary path, validate
# with visudo -c, install at mode 0440 owned root:root.
if [[ "${EVO_INSTALL_NETWORK_NM_SUDOERS:-1}" != "0" ]]; then
    TEMPLATE="$DIST_DIR/sudoers.d/evo-network-nm.in"
    if [[ ! -f "$TEMPLATE" ]]; then
        echo "sudoers template not found at $TEMPLATE" >&2
        exit 2
    fi
    TMP="$(mktemp)"
    trap 'rm -f "$TMP"' EXIT
    sed -e "s|@EVO_SERVICE_USER@|$SERVICE_USER|g" \
        -e "s|/usr/bin/nmcli|$NMCLI_BIN|g" \
        "$TEMPLATE" > "$TMP"

    if ! visudo -c -f "$TMP" >/dev/null; then
        echo "rendered sudoers fragment failed visudo -c; refusing to install" >&2
        echo "  rendered file kept at $TMP for inspection" >&2
        trap - EXIT
        exit 2
    fi

    install -m 0440 -o root -g root "$TMP" "$NETWORK_NM_SUDOERS_FILE"
    rm -f "$TMP"
    trap - EXIT
    echo "[bootstrap] installed $NETWORK_NM_SUDOERS_FILE"
else
    echo "[bootstrap] EVO_INSTALL_NETWORK_NM_SUDOERS=0 — skipping network.nm sudoers drop-in"
fi

# ----------------------------------------------------------
# Step 1c: /etc/sudoers.d/evo-hardware-audio (narrow NOPASSWD)
# ----------------------------------------------------------
# Path-scoped grant for the hardware.audio-config plugin's
# boot-config + i2c-dev module-load.d drop-in writes. The plugin
# never runs as root; the grant is scoped to exactly the two
# boot-config locations + the i2c-dev drop-in path so the audit
# log keeps a full record of any command outside this surface.
if [[ "${EVO_INSTALL_HARDWARE_AUDIO_SUDOERS:-1}" != "0" ]]; then
    TEMPLATE="$DIST_DIR/sudoers.d/evo-hardware-audio.in"
    if [[ ! -f "$TEMPLATE" ]]; then
        echo "sudoers template not found at $TEMPLATE" >&2
        exit 2
    fi
    TMP="$(mktemp)"
    trap 'rm -f "$TMP"' EXIT
    sed -e "s|@EVO_SERVICE_USER@|$SERVICE_USER|g" \
        "$TEMPLATE" > "$TMP"

    if ! visudo -c -f "$TMP" >/dev/null; then
        echo "rendered sudoers fragment failed visudo -c; refusing to install" >&2
        echo "  rendered file kept at $TMP for inspection" >&2
        trap - EXIT
        exit 2
    fi

    install -m 0440 -o root -g root "$TMP" "$HARDWARE_AUDIO_SUDOERS_FILE"
    rm -f "$TMP"
    trap - EXIT
    echo "[bootstrap] installed $HARDWARE_AUDIO_SUDOERS_FILE"
else
    echo "[bootstrap] EVO_INSTALL_HARDWARE_AUDIO_SUDOERS=0 — skipping hardware-audio sudoers drop-in"
fi

# ----------------------------------------------------------
# Step 1d: /etc/sudoers.d/evo-system-power (narrow NOPASSWD)
# ----------------------------------------------------------
# Path-scoped grant for the org.evoframework.system.power plugin's
# `reboot_device` + `power_off_device` verbs. The framework
# dispatcher's per-verb capability gate (step_up:system_admin)
# is the FIRST line of defence — the sudoers grant is the LAST.
# Both Cmnd_Aliases are exact-match (one binary, one argv element
# each); there is no broad sudo grant on this surface.
if [[ "${EVO_INSTALL_SYSTEM_POWER_SUDOERS:-1}" != "0" ]]; then
    TEMPLATE="$DIST_DIR/sudoers.d/evo-system-power.in"
    if [[ ! -f "$TEMPLATE" ]]; then
        echo "sudoers template not found at $TEMPLATE" >&2
        exit 2
    fi
    TMP="$(mktemp)"
    trap 'rm -f "$TMP"' EXIT
    sed -e "s|@EVO_SERVICE_USER@|$SERVICE_USER|g" \
        "$TEMPLATE" > "$TMP"

    if ! visudo -c -f "$TMP" >/dev/null; then
        echo "rendered sudoers fragment failed visudo -c; refusing to install" >&2
        echo "  rendered file kept at $TMP for inspection" >&2
        trap - EXIT
        exit 2
    fi

    install -m 0440 -o root -g root "$TMP" "$SYSTEM_POWER_SUDOERS_FILE"
    rm -f "$TMP"
    trap - EXIT
    echo "[bootstrap] installed $SYSTEM_POWER_SUDOERS_FILE"
else
    echo "[bootstrap] EVO_INSTALL_SYSTEM_POWER_SUDOERS=0 — skipping system-power sudoers drop-in"
fi

# ----------------------------------------------------------
# Step 1d: /usr/share/evo-device-audio/dacs.json
# ----------------------------------------------------------
# The hardware.audio-config plugin embeds the catalogue at build
# time (include_str! against plugins/.../data/import/volumio-dacs.json). The
# canonical on-disk copy lives at /usr/share/evo-device-audio/
# so an OOP-shipped variant of the plugin (or other consumers
# that want the catalogue without linking the lib crate) can
# read the same source. The plugin reads the embedded copy
# first; this file is a documented host-side artifact.
if [[ "${EVO_INSTALL_DACS_CATALOGUE:-1}" != "0" ]]; then
    DACS_CATALOGUE_SOURCE="$DIST_DIR/../plugins/org.evoframework.hardware.audio-config/data/import/volumio-dacs.json"
    if [[ -f "$DACS_CATALOGUE_SOURCE" ]]; then
        install -d -m 0755 -o root -g root "$DACS_CATALOGUE_DIR"
        install -m 0644 -o root -g root \
            "$DACS_CATALOGUE_SOURCE" "$DACS_CATALOGUE_PATH"
        echo "[bootstrap] installed $DACS_CATALOGUE_PATH"
    else
        echo "[bootstrap] WARN: dacs.json source not found at $DACS_CATALOGUE_SOURCE; skipping"
    fi
else
    echo "[bootstrap] EVO_INSTALL_DACS_CATALOGUE=0 — skipping DAC catalogue install"
fi

# ----------------------------------------------------------
# Step 2: systemd drop-ins for the steward unit
# ----------------------------------------------------------
if [[ "${EVO_INSTALL_SYSTEMD_DROP_INS:-1}" != "0" && "$SKIP_SYSTEMD" == "0" ]]; then
    install -d -m 0755 "$SYSTEMD_DROPIN_DIR"
    install -m 0644 -o root -g root \
        "$DIST_DIR/systemd/evo.service.d/exec-start.conf" \
        "$SYSTEMD_DROPIN_DIR/exec-start.conf"
    echo "[bootstrap] installed $SYSTEMD_DROPIN_DIR/exec-start.conf"

    install -m 0644 -o root -g root \
        "$DIST_DIR/systemd/evo.service.d/state-dir-mode.conf" \
        "$SYSTEMD_DROPIN_DIR/state-dir-mode.conf"
    echo "[bootstrap] installed $SYSTEMD_DROPIN_DIR/state-dir-mode.conf"

    install -m 0644 -o root -g root \
        "$DIST_DIR/systemd/evo.service.d/mpd-restart-privileges.conf" \
        "$SYSTEMD_DROPIN_DIR/mpd-restart-privileges.conf"
    echo "[bootstrap] installed $SYSTEMD_DROPIN_DIR/mpd-restart-privileges.conf"

    install -m 0644 -o root -g root \
        "$DIST_DIR/systemd/evo.service.d/hardware-audio-privileges.conf" \
        "$SYSTEMD_DROPIN_DIR/hardware-audio-privileges.conf"
    echo "[bootstrap] installed $SYSTEMD_DROPIN_DIR/hardware-audio-privileges.conf"

    install -m 0644 -o root -g root \
        "$DIST_DIR/systemd/evo.service.d/https.conf" \
        "$SYSTEMD_DROPIN_DIR/https.conf"
    echo "[bootstrap] installed $SYSTEMD_DROPIN_DIR/https.conf"

    "$SYSTEMCTL_BIN" daemon-reload
    echo "[bootstrap] systemctl daemon-reload"
else
    echo "[bootstrap] systemd drop-ins skipped (EVO_INSTALL_SYSTEMD_DROP_INS=0 or --skip-systemd)"
fi

# ----------------------------------------------------------
# Step 2.6: /etc/evo/plugins.d/ — distribution-tier plugin configs
# ----------------------------------------------------------
# The audio reference distribution ships per-plugin default
# configurations under /etc/evo/plugins.d/. Each file is the
# plugin's distribution-tier default; the steward reads these at
# admission time and merges them with operator overrides.
#
# Per the connectivity-check redesign declares that the network plugin's distribution-tier default
# declares `probe_kind = "off"` — no third-party connectivity
# probing without explicit operator opt-in.
#
# The multi-room plugin's distribution-tier default is rendered from
# `--multiroom-role` / `--multiroom-group-id` /
# `--multiroom-source-pcm` / `--multiroom-alsa-pcm` flags. When
# `--multiroom-role` is unset, no multi-room config is written
# (the operator configures it later via the UI's first-boot
# wizard or by re-running the bootstrap with the flags).
install -d -m 0755 -o root -g root "$PLUGINS_D_DIR"
chown "$SERVICE_USER:$SERVICE_USER" "$PLUGINS_D_DIR"

# 2.6a — network plugin distribution-tier default
if [[ "${EVO_INSTALL_NETWORK_PLUGIN_CONFIG:-1}" != "0" ]]; then
    NETWORK_PLUGIN_TEMPLATE="$DIST_DIR/plugins.d/org.evoframework.network.toml"
    NETWORK_PLUGIN_PATH="$PLUGINS_D_DIR/org.evoframework.network.toml"
    if [[ -f "$NETWORK_PLUGIN_TEMPLATE" ]]; then
        install -m 0644 -o "$SERVICE_USER" -g "$SERVICE_USER" \
            "$NETWORK_PLUGIN_TEMPLATE" "$NETWORK_PLUGIN_PATH"
        echo "[bootstrap] installed $NETWORK_PLUGIN_PATH (probe_kind=off per the connectivity-check redesign)"
    else
        echo "[bootstrap] WARN: network plugin template not found at $NETWORK_PLUGIN_TEMPLATE; skipping"
    fi
else
    echo "[bootstrap] EVO_INSTALL_NETWORK_PLUGIN_CONFIG=0 — skipping network plugin config"
fi

# 2.6b — multiroom plugin distribution-tier default
#
# Install the unconditional default (role=auto, no group, no
# PCM beyond the distribution-standard `alsa_pcm = "evo"`)
# so every freshly-installed device admits the multiroom
# plugin against an explicit on-disk config file rather than
# against the plugin's internal Default impl. The
# operator-gestured `--multiroom-role=...` path below
# overwrites this default when an explicit role is provided.
# Operator-visible state must live in files the operator can
# read and edit; relying on an implicit code-side Default is
# a configuration smell that leaves the operator guessing
# what the plugin is doing.
if [[ "${EVO_INSTALL_MULTIROOM_PLUGIN_CONFIG:-1}" != "0" ]]; then
    MULTIROOM_DEFAULT_TEMPLATE="$DIST_DIR/plugins.d/org.evoframework.multiroom.evo-native.toml"
    MULTIROOM_DEFAULT_PATH="$PLUGINS_D_DIR/org.evoframework.multiroom.evo-native.toml"
    if [[ -f "$MULTIROOM_DEFAULT_TEMPLATE" ]]; then
        install -m 0644 -o "$SERVICE_USER" -g "$SERVICE_USER" \
            "$MULTIROOM_DEFAULT_TEMPLATE" "$MULTIROOM_DEFAULT_PATH"
        echo "[bootstrap] installed $MULTIROOM_DEFAULT_PATH (role=auto by default)"
    else
        echo "[bootstrap] WARN: multiroom plugin default not found at $MULTIROOM_DEFAULT_TEMPLATE; skipping"
    fi
else
    echo "[bootstrap] EVO_INSTALL_MULTIROOM_PLUGIN_CONFIG=0 — skipping multiroom plugin default config"
fi

# 2.6c — multiroom plugin config (rendered from flags)
#
# Convert a comma-separated list ("a,b,c") to a TOML array
# literal (["a", "b", "c"]). Trims whitespace around each
# element; emits an empty array literal for an empty input.
# Used to render --multiroom-group-members and
# --multiroom-group-member-addresses into the template.
csv_to_toml_array() {
    local input="$1"
    if [[ -z "$input" ]]; then
        echo "[]"
        return
    fi
    local out="["
    local first=1
    local item
    local IFS=','
    for item in $input; do
        # Trim surrounding whitespace.
        item="${item#"${item%%[![:space:]]*}"}"
        item="${item%"${item##*[![:space:]]}"}"
        if [[ $first -eq 1 ]]; then
            first=0
        else
            out+=", "
        fi
        out+="\"$item\""
    done
    out+="]"
    echo "$out"
}

if [[ -n "$MULTIROOM_ROLE" ]]; then
    case "$MULTIROOM_ROLE" in
        source|receiver|auto) ;;
        *)
            echo "--multiroom-role must be one of: source, receiver, auto (got: $MULTIROOM_ROLE)" >&2
            echo "" >&2
            echo "Note: 'none' was previously accepted but produced TOML the plugin rejected at admission" >&2
            echo "      (the plugin's Role enum has no None variant). Use 'auto' for a device that should" >&2
            echo "      stay non-engaged unless the operator later sets a role." >&2
            exit 1
            ;;
    esac
    if [[ -z "$MULTIROOM_GROUP_ID" ]]; then
        echo "--multiroom-role $MULTIROOM_ROLE requires --multiroom-group-id <uuid>" >&2
        exit 1
    fi
    MULTIROOM_TEMPLATE="$DIST_DIR/plugins.d/org.evoframework.multiroom.evo-native.toml.in"
    MULTIROOM_PATH="$PLUGINS_D_DIR/org.evoframework.multiroom.evo-native.toml"
    if [[ ! -f "$MULTIROOM_TEMPLATE" ]]; then
        echo "multiroom template not found at $MULTIROOM_TEMPLATE" >&2
        exit 2
    fi
    # Source role mandates the receiver-list pair. The plugin's
    # apply_config refuses to load a source-role config missing
    # either field; fail at bootstrap time with the same
    # contract so operator-visible breakage stays at install
    # time, not at first plugin admission.
    if [[ "$MULTIROOM_ROLE" == "source" ]]; then
        if [[ -z "$MULTIROOM_GROUP_MEMBERS" ]]; then
            echo "--multiroom-role source requires --multiroom-group-members \"<device-id>,<device-id>,...\"" >&2
            exit 1
        fi
        if [[ -z "$MULTIROOM_GROUP_MEMBER_ADDRESSES" ]]; then
            echo "--multiroom-role source requires --multiroom-group-member-addresses \"<host>:<port>,<host>:<port>,...\"" >&2
            exit 1
        fi
    fi
    case "$MULTIROOM_ROLE" in
        source)
            if [[ -z "$MULTIROOM_SOURCE_PCM" ]]; then
                echo "--multiroom-role source requires --multiroom-source-pcm <alsa-pcm>" >&2
                exit 1
            fi
            # Source role runs one-renderer semantics: MPD writes
            # to source_pcm's producer path, while multiroom owns
            # local DAC writes via alsa_pcm. If the operator does
            # not supply --multiroom-alsa-pcm, default to the
            # source template's dedicated local-renderer alias.
            if [[ -z "$MULTIROOM_ALSA_PCM" ]]; then
                MULTIROOM_ALSA_PCM="evo_local"
                echo "[bootstrap] --multiroom-alsa-pcm unset for source role; defaulting to $MULTIROOM_ALSA_PCM"
            fi
            MULTIROOM_ALSA_PCM_LINE="alsa_pcm = \"$MULTIROOM_ALSA_PCM\""
            MULTIROOM_SOURCE_PCM_LINE="source_pcm = \"$MULTIROOM_SOURCE_PCM\""
            MULTIROOM_GROUP_MEMBERS_LINE="group_members = $(csv_to_toml_array "$MULTIROOM_GROUP_MEMBERS")"
            MULTIROOM_GROUP_MEMBER_ADDRESSES_LINE="group_member_addresses = $(csv_to_toml_array "$MULTIROOM_GROUP_MEMBER_ADDRESSES")"
            ;;
        receiver)
            if [[ -z "$MULTIROOM_ALSA_PCM" ]]; then
                echo "--multiroom-role receiver requires --multiroom-alsa-pcm <alsa-pcm>" >&2
                exit 1
            fi
            MULTIROOM_ALSA_PCM_LINE="alsa_pcm = \"$MULTIROOM_ALSA_PCM\""
            MULTIROOM_SOURCE_PCM_LINE="# source_pcm (source role only)"
            # group_members + group_member_addresses are source-
            # role-only. Receivers render every frame that
            # arrives on the audio plane regardless of group, so
            # they do not need the membership list. Emit empty
            # placeholder lines (the comment keeps the line non-
            # empty so the template stays well-formed).
            MULTIROOM_GROUP_MEMBERS_LINE="# group_members (source role only)"
            MULTIROOM_GROUP_MEMBER_ADDRESSES_LINE="# group_member_addresses (source role only)"
            ;;
        auto)
            MULTIROOM_ALSA_PCM_LINE="# no alsa_pcm (role=auto — plugin defers PCM choice until engaged)"
            MULTIROOM_SOURCE_PCM_LINE="# no source_pcm (role=auto — plugin defers PCM choice until engaged)"
            MULTIROOM_GROUP_MEMBERS_LINE="# group_members (source role only)"
            MULTIROOM_GROUP_MEMBER_ADDRESSES_LINE="# group_member_addresses (source role only)"
            ;;
    esac
    # Preservation gate: when EVO_PRESERVE_MULTIROOM_TOML=1 and
    # the target file already exists, skip the overwrite so
    # operator-tuned member lists survive bootstrap re-runs
    # (e.g. on binary upgrade). Default behaviour (unset or 0)
    # overwrites unconditionally — matches the fresh-install
    # ergonomic. The plugin's load-time validation still kicks
    # in on the preserved file, so a stale source-role config
    # missing the new fields fails loudly on next admission.
    if [[ -f "$MULTIROOM_PATH" \
        && "${EVO_PRESERVE_MULTIROOM_TOML:-0}" == "1" ]]; then
        echo "[bootstrap] preserved existing $MULTIROOM_PATH (EVO_PRESERVE_MULTIROOM_TOML=1)"
    else
        TMP="$(mktemp)"
        trap 'rm -f "$TMP"' EXIT
        sed -e "s|@MULTIROOM_ROLE@|$MULTIROOM_ROLE|g" \
            -e "s|@MULTIROOM_GROUP_ID@|$MULTIROOM_GROUP_ID|g" \
            -e "s|@MULTIROOM_GROUP_MEMBERS_LINE@|$MULTIROOM_GROUP_MEMBERS_LINE|g" \
            -e "s|@MULTIROOM_GROUP_MEMBER_ADDRESSES_LINE@|$MULTIROOM_GROUP_MEMBER_ADDRESSES_LINE|g" \
            -e "s|@MULTIROOM_ALSA_PCM_LINE@|$MULTIROOM_ALSA_PCM_LINE|g" \
            -e "s|@MULTIROOM_SOURCE_PCM_LINE@|$MULTIROOM_SOURCE_PCM_LINE|g" \
            "$MULTIROOM_TEMPLATE" > "$TMP"
        install -m 0644 -o "$SERVICE_USER" -g "$SERVICE_USER" \
            "$TMP" "$MULTIROOM_PATH"
        rm -f "$TMP"
        trap - EXIT
        echo "[bootstrap] installed $MULTIROOM_PATH (role=$MULTIROOM_ROLE, group=$MULTIROOM_GROUP_ID)"
    fi
else
    echo "[bootstrap] --multiroom-role unset — skipping multiroom plugin config (configure later via UI wizard or re-run bootstrap with --multiroom-role)"
fi

# ----------------------------------------------------------
# Step 2.7: disable avahi-daemon (it fights evo's own mDNS on 5353)
# ----------------------------------------------------------
# The evo steward binds its own mDNS responder to UDP 5353 for
# multi-room peer discovery. avahi-daemon (commonly installed on
# Debian / Raspberry Pi OS) also binds 5353 by default; the two
# fight for the multicast group and audio-plane peer discovery
# becomes flaky. Default behaviour: stop + disable avahi-daemon
# when present. Vendor distributions that need avahi for non-evo
# services flip `EVO_DISABLE_AVAHI=0`.
if [[ "${EVO_DISABLE_AVAHI:-1}" != "0" ]]; then
    if "$SYSTEMCTL_BIN" list-unit-files 2>/dev/null \
        | grep -q '^avahi-daemon\.service'; then
        if "$SYSTEMCTL_BIN" is-active --quiet avahi-daemon.service 2>/dev/null \
            || "$SYSTEMCTL_BIN" is-enabled --quiet avahi-daemon.service \
                2>/dev/null; then
            "$SYSTEMCTL_BIN" disable --now avahi-daemon.service \
                >/dev/null 2>&1 || true
            "$SYSTEMCTL_BIN" disable --now avahi-daemon.socket \
                >/dev/null 2>&1 || true
            echo "[bootstrap] disabled avahi-daemon (evo binds UDP 5353 directly)"
        else
            echo "[bootstrap] avahi-daemon already inactive + disabled"
        fi
    else
        echo "[bootstrap] avahi-daemon not present — no action"
    fi
else
    echo "[bootstrap] EVO_DISABLE_AVAHI=0 — avahi-daemon left as-is"
fi

# ----------------------------------------------------------
# Step 2.8: /etc/evo/trust.d/ — distribution-tier plugin trust
# ----------------------------------------------------------
# Out-of-process plugins are admitted from
# `/opt/evo/plugins/<name>/` only after their bundle signature
# verifies against a public key the steward has loaded from
# `/etc/evo/trust.d/`. The framework's prototype-install.sh
# seeds framework-tier roots (e.g. release-signing); the
# distribution layers its own plugin trust on top.
#
# The audio reference distribution publishes its plugins under
# the `org.evoframework.*` namespace; the commons-plugin
# signing key authorises that namespace at trust class
# `platform`. The matching public key + sidecar ship in
# `dist/keys/` and install here so a fresh-OS install lands at
# the working trust posture without operator key juggling.
COMMONS_TRUST_PEM="$DIST_DIR/keys/commons-plugin-signing-public.pem"
COMMONS_TRUST_META="$DIST_DIR/keys/commons-plugin-signing-public.meta.toml"
TRUST_D_DIR="/etc/evo/trust.d"
if [[ -f "$COMMONS_TRUST_PEM" && -f "$COMMONS_TRUST_META" ]]; then
    install -d -m 0755 -o root -g root "$TRUST_D_DIR"
    install -m 0644 -o root -g root \
        "$COMMONS_TRUST_PEM" "$TRUST_D_DIR/commons-plugin-signing-public.pem"
    install -m 0644 -o root -g root \
        "$COMMONS_TRUST_META" "$TRUST_D_DIR/commons-plugin-signing-public.meta.toml"
    echo "[bootstrap] installed $TRUST_D_DIR/commons-plugin-signing-public.{pem,meta.toml}"
else
    echo "[bootstrap] WARN: commons-plugin trust files missing from $DIST_DIR/keys/ — out-of-process plugin admission will fail until installed" >&2
fi

# ----------------------------------------------------------
# Step 2.9: /opt/evo/plugins/ — search root for OOP plugin bundles
# ----------------------------------------------------------
# The framework's Phase 2 plugin discovery walks
# `plugins.search_roots` (default `/opt/evo/plugins` then
# `/var/lib/evo/plugins`). Ensure the canonical first root
# exists; bundle install (via `deploy-distribution.sh` or
# operator-driven `evo-plugin-tool plugin install`) lands
# `<plugin-name>/manifest.toml` + `plugin.bin` + `manifest.sig`
# under this root.
install -d -m 0755 -o root -g root /opt/evo/plugins
echo "[bootstrap] /opt/evo/plugins/ created (mode 0755, plugin bundle search root)"

# ----------------------------------------------------------
# Step 2.5: /etc/evo/client_acl.toml — operator capability ACL
# ----------------------------------------------------------
# The framework's wire-surface ACL gates plans_admin /
# plugins_admin / reconciliation_admin / grammar_admin
# capabilities behind operator-controlled policy. Absent file =
# default-deny posture; operator wiring `evo-plugin-tool` over
# the local socket would be refused until this file is in
# place. Toggle off via EVO_INSTALL_CLIENT_ACL=0 for vendor
# distributions composing their own ACL externally.
if [[ "${EVO_INSTALL_CLIENT_ACL:-1}" != "0" ]]; then
    CLIENT_ACL_TEMPLATE="$DIST_DIR/etc-evo/client_acl.toml"
    CLIENT_ACL_PATH="/etc/evo/client_acl.toml"
    if [[ ! -f "$CLIENT_ACL_TEMPLATE" ]]; then
        echo "client_acl template not found at $CLIENT_ACL_TEMPLATE" >&2
        exit 2
    fi
    install -d -m 0755 -o root -g root "$(dirname "$CLIENT_ACL_PATH")"
    if [[ -f "$CLIENT_ACL_PATH" ]] && \
       ! cmp -s "$CLIENT_ACL_TEMPLATE" "$CLIENT_ACL_PATH"; then
        backup="$CLIENT_ACL_PATH.pre-evo.$(date +%Y%m%d%H%M%S)"
        cp -a "$CLIENT_ACL_PATH" "$backup"
        echo "[bootstrap] backed up prior $CLIENT_ACL_PATH to $backup"
    fi
    install -m 0644 -o root -g root \
        "$CLIENT_ACL_TEMPLATE" "$CLIENT_ACL_PATH"
    echo "[bootstrap] installed $CLIENT_ACL_PATH"
else
    echo "[bootstrap] EVO_INSTALL_CLIENT_ACL=0 — skipping client_acl"
fi

# ----------------------------------------------------------
# Step 3: /etc/evo/mpd.conf — boot-time fragment owned by service user
# ----------------------------------------------------------
if [[ "${EVO_INSTALL_MPD_FRAGMENT:-1}" != "0" ]]; then
    FRAGMENT_PARENT="$(dirname "$MPD_FRAGMENT_PATH")"
    install -d -m 0755 -o root -g root "$FRAGMENT_PARENT"
    # The fragment-writer worker uses atomic-write (stage at
    # .mpd.conf.tmp, fsync, rename) so the service user needs
    # WRITE permission on the PARENT directory, not just the
    # fragment file. chown the parent so creating the staging
    # file works without the worker needing extra privileges.
    # Sibling root-owned files (client_acl.toml, trust.d/)
    # stay untouched per their own ownership.
    chown "$SERVICE_USER:$SERVICE_USER" "$FRAGMENT_PARENT"
    echo "[bootstrap] $FRAGMENT_PARENT owned by $SERVICE_USER (mode 0755)"
    # Seed with the static modular-pipeline fragment (device
    # "evo" -> /etc/asound.conf -> hardware). The
    # plugin's fragment-writer worker overwrites this on every
    # route change once the framework publishes a topology;
    # the static form gives MPD a valid audio_output at boot
    # before any topology is resolved.
    FRAGMENT_TEMPLATE="$DIST_DIR/mpd/evo-fragment.conf"
    if [[ -f "$FRAGMENT_TEMPLATE" ]]; then
        install -m 0644 -o "$SERVICE_USER" -g "$SERVICE_USER" \
            "$FRAGMENT_TEMPLATE" "$MPD_FRAGMENT_PATH"
    else
        : > "$MPD_FRAGMENT_PATH"
        chown "$SERVICE_USER:$SERVICE_USER" "$MPD_FRAGMENT_PATH"
        chmod 0644 "$MPD_FRAGMENT_PATH"
    fi
    echo "[bootstrap] $MPD_FRAGMENT_PATH owned by $SERVICE_USER (mode 0644)"
else
    echo "[bootstrap] EVO_INSTALL_MPD_FRAGMENT=0 — skipping fragment-path chown"
fi

# ----------------------------------------------------------
# Step 3.6: /var/lib/evo/music/{INTERNAL,USB,NAS} +
# /etc/mpd.conf music_directory pin
# ----------------------------------------------------------
# The audio distribution's prescribed music library layout
# (documented in dist/README.md; gated as a release-cut
# acceptance criterion). The directories must exist BEFORE
# the mpd restart in the asound step so mpd's startup
# music_directory open succeeds — otherwise mpd logs an
# exception and the post-install gate's zero-fail invariant
# fails. Boilerplate per the distribution: same on every
# target, every install.
#
# Ownership: owner SERVICE_USER, group `audio` when present
# (Debian convention for music-library access), with a
# SERVICE_USER:SERVICE_USER fallback for hosts where the
# `audio` group does not exist.
if [[ "${EVO_INSTALL_MUSIC_LIBRARY:-1}" != "0" ]]; then
    install -d -m 0755 -o root -g root /var/lib/evo
    if ! install -d -m 0755 -o "$SERVICE_USER" -g audio \
            /var/lib/evo/music \
            /var/lib/evo/music/INTERNAL \
            /var/lib/evo/music/USB \
            /var/lib/evo/music/NAS 2>/dev/null; then
        install -d -m 0755 -o "$SERVICE_USER" -g "$SERVICE_USER" \
            /var/lib/evo/music \
            /var/lib/evo/music/INTERNAL \
            /var/lib/evo/music/USB \
            /var/lib/evo/music/NAS
    fi
    echo "[bootstrap] /var/lib/evo/music/{INTERNAL,USB,NAS} ensured (owner $SERVICE_USER, mode 0755)"
    # mpd's music_directory must point at /var/lib/evo/music
    # before the restart later in this script. The line is
    # in /etc/mpd.conf (Debian shape: top-level
    # `music_directory "..."`). idempotent: rewrite only when
    # the current value differs.
    if [[ -f /etc/mpd.conf ]] \
        && ! grep -qE '^\s*music_directory\s+"/var/lib/evo/music"' /etc/mpd.conf; then
        sed -i.pre-evo-music -E \
            's|^\s*music_directory\s+".*"|music_directory "/var/lib/evo/music"|' \
            /etc/mpd.conf
        echo "[bootstrap] pinned music_directory in /etc/mpd.conf to /var/lib/evo/music"
    fi
else
    echo "[bootstrap] EVO_INSTALL_MUSIC_LIBRARY=0 — skipping music library skeleton + music_directory pin"
fi

# ----------------------------------------------------------
# Step 3.5: /opt/evo/catalogue/default.toml — distribution
# catalogue including this audio-rack fragment. The catalogue
# composer is intentionally minimal in this build: it
# overwrites the existing catalogue at the canonical install
# path with the dist's audio-rack.toml AS-IS — the framework's
# validation distribution catalogue (which the framework
# release ships) is replaced by the audio distribution's
# catalogue. Vendor distributions that compose racks from
# multiple sources override `EVO_INSTALL_CATALOGUE=0` and
# handle composition externally.
# ----------------------------------------------------------
if [[ "${EVO_INSTALL_CATALOGUE:-1}" != "0" ]]; then
    CATALOGUE_TEMPLATE="$DIST_DIR/catalogue/audio-rack.toml"
    CATALOGUE_PATH="/opt/evo/catalogue/default.toml"
    if [[ ! -f "$CATALOGUE_TEMPLATE" ]]; then
        echo "catalogue template not found at $CATALOGUE_TEMPLATE" >&2
        exit 2
    fi
    install -d -m 0755 -o root -g root "$(dirname "$CATALOGUE_PATH")"
    if [[ -f "$CATALOGUE_PATH" ]] && \
       ! cmp -s "$CATALOGUE_TEMPLATE" "$CATALOGUE_PATH"; then
        backup="$CATALOGUE_PATH.pre-evo.$(date +%Y%m%d%H%M%S)"
        cp -a "$CATALOGUE_PATH" "$backup"
        echo "[bootstrap] backed up prior $CATALOGUE_PATH to $backup"
    fi
    # The audio-rack.toml dist fragment is NOT a complete
    # catalogue — it omits schema_version on purpose so it can
    # be included from a larger catalogue. Render a complete
    # form by prepending schema_version = 1.
    TMP_CAT=$(mktemp)
    {
        echo "# Composed by dist/scripts/bootstrap.sh on $(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "# Source fragment: $CATALOGUE_TEMPLATE"
        echo "# Vendor distributions compose differently; this is the"
        echo "# audio-only reference."
        echo
        echo "schema_version = 1"
        echo
        cat "$CATALOGUE_TEMPLATE"
    } > "$TMP_CAT"
    install -m 0644 -o root -g root "$TMP_CAT" "$CATALOGUE_PATH"
    rm -f "$TMP_CAT"
    echo "[bootstrap] installed $CATALOGUE_PATH"
else
    echo "[bootstrap] EVO_INSTALL_CATALOGUE=0 — skipping catalogue install"
fi

# ----------------------------------------------------------
# Step 3.7: inject `include "/etc/evo/mpd.conf"` into the
# distro's /etc/mpd.conf so MPD reads the audio_output block
# the audio reference distribution ships at $MPD_FRAGMENT_PATH.
#
# Why this step exists: Debian's mpd package writes
# /etc/mpd.conf at install with NO audio_output block; MPD's
# auto-detection then picks the first plugin that probes
# successfully (often `sndio` on Debian — a plugin that
# claims to detect a device even when the sndio daemon is
# absent, causing playback to fail at first play). Injecting
# the include wires MPD to the audio dist's own
# audio_output block, eliminating the auto-detect race.
#
# Idempotent: a sentinel-delimited block marks the injection
# so re-running replaces the block in place rather than
# stacking duplicates. Operators that prefer the
# MPDCONF=/etc/evo/mpd.conf shape (set in /etc/default/mpd)
# disable this step via EVO_INSTALL_MPD_INCLUDE=0 and manage
# the merge externally.
# ----------------------------------------------------------
if [[ "${EVO_INSTALL_MPD_INCLUDE:-1}" != "0" ]]; then
    if [[ ! -f "$MPD_CONF_PATH" ]]; then
        echo "  [skip]  $MPD_CONF_PATH absent — install mpd package or set EVO_INSTALL_MPD_INCLUDE=0" >&2
    else
        SENTINEL_BEGIN="# >>> evo-device-audio (bootstrap.sh) — DO NOT EDIT >>>"
        SENTINEL_END="# <<< evo-device-audio (bootstrap.sh) — DO NOT EDIT <<<"
        # Strip any prior block (idempotent re-run).
        TMP_MPD="$(mktemp)"
        trap 'rm -f "$TMP_MPD"' EXIT
        awk -v b="$SENTINEL_BEGIN" -v e="$SENTINEL_END" '
            $0 == b { in_block = 1; next }
            $0 == e { in_block = 0; next }
            !in_block { print }
        ' "$MPD_CONF_PATH" > "$TMP_MPD"
        # Append the fresh sentinel-delimited include block.
        {
            cat "$TMP_MPD"
            echo
            echo "$SENTINEL_BEGIN"
            echo "include \"$MPD_FRAGMENT_PATH\""
            echo "$SENTINEL_END"
        } > "$TMP_MPD.new"
        # Preserve original owner/mode; the file is root-owned
        # mode 0644 on Debian.
        install -m 0644 -o root -g root "$TMP_MPD.new" "$MPD_CONF_PATH"
        rm -f "$TMP_MPD" "$TMP_MPD.new"
        trap - EXIT
        echo "[bootstrap] injected include \"$MPD_FRAGMENT_PATH\" into $MPD_CONF_PATH"
    fi
else
    echo "[bootstrap] EVO_INSTALL_MPD_INCLUDE=0 — skipping mpd.conf include injection"
fi

# ----------------------------------------------------------
# Step 4: /etc/asound.conf — modular ALSA pipeline (pcm.evo)
#
# A single base template ships in dist/alsa/:
#
#   - asound.conf — pcm.evo writes straight to the local DAC,
#                   identical on every device regardless of
#                   multi-room configuration. This is the
#                   bit-perfect local-playback floor: the
#                   behaviour the device exhibits when no
#                   multi-room layer is engaged, the same on
#                   solo devices and on devices configured for
#                   any multi-room role. ALSA's `last definition
#                   wins` semantics let runtime drop-ins under
#                   /etc/asound.d/ compose additional pipeline
#                   stages (resampler, EQ, room correction,
#                   multi-room loopback) on top, and removing a
#                   drop-in collapses the chain back to this
#                   base unchanged.
#
# The multi-room plugin owns its own runtime drop-in at
# `/etc/asound.d/10-evo-multiroom-source.conf` (written and
# removed by the plugin's source-mode engagement code, not by
# bootstrap). The plugin's drop-in redefines pcm.evo to route
# through snd-aloop only while a live group is engaged; the
# instant the group goes away the drop-in is removed and pcm.evo
# resolves to the base direct path on the next PCM open. The
# playback.mpd plugin's supervisor inotify-watches
# /etc/asound.d/ and cycles MPD output (disableoutput +
# enableoutput) on every change so MPD picks up the
# composition shift without a systemd bounce.
#
# `snd-aloop` is loaded and persisted unconditionally here. The
# kernel module is cheap when idle (a few KB resident, no open
# substreams until the plugin engages source mode) and being
# present on demand is what lets the multi-room plugin engage
# source mode without re-bootstrapping.
# ----------------------------------------------------------
if [[ "${EVO_INSTALL_ASOUND_CONF:-1}" != "0" ]]; then
    ASOUND_TEMPLATE="$DIST_DIR/alsa/asound.conf"
    # snd-aloop is required for the multi-room plugin's
    # source-mode runtime drop-in. Load now AND persist via
    # /etc/modules-load.d/ so the module reloads across reboots
    # without operator intervention. Unconditional because the
    # plugin engages source mode at runtime, not at install
    # time; the install record cannot predict which devices
    # will eventually act as a source.
    if ! lsmod | grep -q '^snd_aloop'; then
        modprobe snd-aloop
        echo "[bootstrap] loaded snd-aloop kernel module"
    fi
    install -d -m 0755 /etc/modules-load.d
    echo "snd-aloop" > /etc/modules-load.d/evo-snd-aloop.conf
    chmod 0644 /etc/modules-load.d/evo-snd-aloop.conf
    echo "[bootstrap] persisted snd-aloop in /etc/modules-load.d/evo-snd-aloop.conf"
    if [[ ! -f "$ASOUND_TEMPLATE" ]]; then
        echo "asound template not found at $ASOUND_TEMPLATE" >&2
        exit 2
    fi
    # Render the template, substituting @EVO_AUDIO_CARD@ with
    # the operator's (or auto-detected) card name. The template
    # ships the placeholder so the bootstrap is the single
    # authoritative point of substitution; vendor distributions
    # that re-template differently swap out this step.
    ASOUND_RENDERED="$(mktemp)"
    trap 'rm -f "$ASOUND_RENDERED"' EXIT
    sed -e "s|@EVO_AUDIO_CARD@|$AUDIO_CARD|g" \
        "$ASOUND_TEMPLATE" > "$ASOUND_RENDERED"
    # Placeholder-residue invariant: if the rendered file still
    # carries any `@SOMETHING@` token, the substitution chain is
    # incomplete (either a new placeholder was added to the
    # template without a matching sed branch, or the template
    # was modified after this script). A silent install leaves
    # an unusable file on disk that surfaces only at first audio
    # play — the failure class that motivated this guard. Refuse
    # to install; the operator gets a clear pointer at the
    # specific token left over.
    if RESIDUE="$(grep -oE '@[A-Z_][A-Z0-9_]*@' "$ASOUND_RENDERED" \
            | sort -u | head -5)"; [[ -n "$RESIDUE" ]]; then
        echo "rendered $ASOUND_TEMPLATE still carries unresolved placeholders:" >&2
        printf '%s\n' "$RESIDUE" | sed 's/^/  /' >&2
        echo "refusing to install $ASOUND_CONF_PATH (would leave audio unplayable)" >&2
        echo "  rendered file kept at $ASOUND_RENDERED for inspection" >&2
        trap - EXIT
        exit 2
    fi
    # If an existing /etc/asound.conf is present with different
    # contents (compared against the rendered form, not the
    # template), back it up first so the operator never loses
    # state silently. Idempotent: re-running after a clean
    # install does not stack backups.
    if [[ -f "$ASOUND_CONF_PATH" ]] && \
       ! cmp -s "$ASOUND_RENDERED" "$ASOUND_CONF_PATH"; then
        backup="$ASOUND_CONF_PATH.pre-evo.$(date +%Y%m%d%H%M%S)"
        cp -a "$ASOUND_CONF_PATH" "$backup"
        echo "[bootstrap] backed up prior $ASOUND_CONF_PATH to $backup"
    fi
    install -m 0644 -o root -g root "$ASOUND_RENDERED" "$ASOUND_CONF_PATH"
    rm -f "$ASOUND_RENDERED"
    trap - EXIT
    echo "[bootstrap] installed $ASOUND_CONF_PATH (card=$AUDIO_CARD)"
    # ALSA reads /etc/asound.conf at every PCM open, so no
    # daemon reload is needed for ALSA itself. MPD caches the
    # asound state at startup though, so bounce it to pick up
    # the new pcm.evo definition. We are running as root here.
    if "$SYSTEMCTL_BIN" is-active mpd.service >/dev/null 2>&1; then
        "$SYSTEMCTL_BIN" restart mpd.service
        echo "[bootstrap] restarted mpd.service to pick up pcm.evo"
    fi

    # Create /etc/asound.d/ owned by the steward service user
    # so the delivery.alsa plugin can atomic-write the
    # operator-options drop-in at runtime without sudoers
    # escalation. The drop-in path is the canonical override
    # surface for operator-tunable pcm.evo settings
    # (mixer type, output device, resampling target); the base
    # /etc/asound.conf above includes it via the
    # `<configfile>` directive, so ALSA's PCM-open re-read
    # picks up the operator change on the next play /
    # pause-resume cycle.
    #
    # `mode 0775 root:<service-user>` lets the service user
    # write files into the directory while still allowing
    # operator-readable inspection via standard tooling
    # (`cat /etc/asound.d/evo-options.conf`). The directory
    # itself is owned root:<service-user> rather than fully
    # service-owned so an accidental `chown -R` against the
    # service user's home directory cannot reparent the
    # directory and break the drop-in path.
    install -d -m 0775 -o root -g "$SERVICE_USER" /etc/asound.d
    echo "[bootstrap] ensured /etc/asound.d/ (mode 0775, owner root:$SERVICE_USER)"

    # Seed an empty operator-options drop-in so the base
    # asound.conf's `<configfile>` include never fails to
    # parse on a fresh install. The delivery.alsa plugin
    # atomic-overwrites this file at runtime on every operator
    # gesture against the playback.options settings surface;
    # the seed body is a single header comment naming the
    # canonical writer so an operator who reads the file
    # pre-first-edit understands it is plugin-managed.
    EVO_OPTIONS_DROPIN_PATH="/etc/asound.d/evo-options.conf"
    if [[ ! -f "$EVO_OPTIONS_DROPIN_PATH" ]]; then
        {
            echo "# Operator-options ALSA drop-in for evo-device-audio."
            echo "# Plugin-managed: org.evoframework.delivery.alsa rewrites"
            echo "# this file atomically on every operator gesture against"
            echo "# the playback.options settings surface. Empty on a"
            echo "# fresh install (no operator gestures yet); the"
            echo "# bootstrap-installed baseline pcm.evo in"
            echo "# /etc/asound.conf is the active definition until the"
            echo "# first override is written."
        } | install -m 0664 -o root -g "$SERVICE_USER" /dev/stdin \
            "$EVO_OPTIONS_DROPIN_PATH"
        echo "[bootstrap] seeded empty $EVO_OPTIONS_DROPIN_PATH"
    fi
else
    echo "[bootstrap] EVO_INSTALL_ASOUND_CONF=0 — skipping asound.conf"
fi

# ----------------------------------------------------------
# Modder staging directory: /etc/evo/hardware/audio/overlays/
# ----------------------------------------------------------
# The hardware.audio-config plugin's modder workflow persists
# operator-uploaded DTBO overlays + their TOML row metadata to
# this directory. The plugin runs as the steward service user
# and writes both files directly (no sudo) — the directory
# itself is owned root:<service-user> so the plugin can write
# but a casual chown of the service user's home does not
# reparent the data store. Operator places the signed allowlist
# at <dir>/allowlist.signed BEFORE running register_overlay;
# the plugin refuses every register gesture without it.
#
# DTBO blobs install from staging to /boot/firmware/overlays/
# via the narrow sudoers grant (EVO_HARDWARE_AUDIO_MODDER alias
# in /etc/sudoers.d/evo-hardware-audio). The plugin never runs
# as root; the grant is the sole privilege escalation surface
# for the modder workflow.
if [[ "${EVO_INSTALL_MODDER_DIR:-1}" != "0" ]]; then
    install -d -m 0775 -o root -g "$SERVICE_USER" \
        /etc/evo/hardware/audio/overlays
    echo "[bootstrap] ensured /etc/evo/hardware/audio/overlays/ (mode 0775, owner root:$SERVICE_USER)"
else
    echo "[bootstrap] EVO_INSTALL_MODDER_DIR=0 — skipping modder staging directory"
fi

# ----------------------------------------------------------
# Verification: confirm what we just installed.
# ----------------------------------------------------------
echo
echo "[verify] preflight checks:"

# Network plugin config: probe_kind = off connectivity-check-redesign invariant.
if [[ -f /etc/evo/plugins.d/org.evoframework.network.toml ]]; then
    NETWORK_PROBE_KIND="$(grep -E '^probe_kind' \
        /etc/evo/plugins.d/org.evoframework.network.toml 2>/dev/null \
        | head -1 | sed -E 's/.*=\s*"([^"]+)".*/\1/')"
    if [[ "$NETWORK_PROBE_KIND" == "off" ]]; then
        echo "  [ok]    network plugin probe_kind=off (no third-party probing)"
    else
        echo "  [WARN]  network plugin probe_kind='${NETWORK_PROBE_KIND}' (default per the connectivity-check redesign is 'off')"
    fi
else
    echo "  [WARN]  /etc/evo/plugins.d/org.evoframework.network.toml not installed (plugin uses code defaults)"
fi

# Multiroom plugin config: presence + role honesty. The
# distribution-tier default ships unconditionally (role=auto)
# so the missing-file case below should only fire when the
# operator explicitly set EVO_INSTALL_MULTIROOM_PLUGIN_CONFIG=0.
if [[ -f /etc/evo/plugins.d/org.evoframework.multiroom.evo-native.toml ]]; then
    MR_ROLE="$(grep -E '^role' \
        /etc/evo/plugins.d/org.evoframework.multiroom.evo-native.toml \
        2>/dev/null | head -1 | sed -E 's/.*=\s*"([^"]+)".*/\1/')"
    echo "  [ok]    multiroom plugin config installed (role=${MR_ROLE:-?})"
else
    echo "  [WARN]  /etc/evo/plugins.d/org.evoframework.multiroom.evo-native.toml not installed (plugin uses code defaults — set EVO_INSTALL_MULTIROOM_PLUGIN_CONFIG=1 or re-run with --multiroom-role)"
fi

# avahi-daemon must NOT hold UDP 5353 — evo binds it directly.
if "$SYSTEMCTL_BIN" is-active --quiet avahi-daemon.service 2>/dev/null; then
    echo "  [WARN]  avahi-daemon is active — it fights evo's mDNS on UDP 5353"
    echo "          (set EVO_DISABLE_AVAHI=1 and re-run bootstrap, or disable manually)"
else
    echo "  [ok]    avahi-daemon inactive (UDP 5353 free for evo's mDNS responder)"
fi

# ExecStart override resolves to this distribution's steward
# binary. The framework reference unit bakes
# ExecStart=/opt/evo/bin/evo; the exec-start.conf drop-in
# clears that and substitutes /opt/evo/bin/evo-device-audio.
# A bare reference unit (no drop-in, or drop-in skipped)
# would launch the wrong binary on next start. The next-start
# answer is what matters here, not whether the live PID has
# the right binary (that reflects the previous unit state).
EXEC_START_RESOLVED="$("$SYSTEMCTL_BIN" show evo --no-pager -p ExecStart \
    2>/dev/null | sed -n 's/.*path=\([^ ]*\).*/\1/p' | head -1)"
if [[ "$EXEC_START_RESOLVED" == "/opt/evo/bin/evo-device-audio" ]]; then
    echo "  [ok]    evo.service ExecStart resolves to /opt/evo/bin/evo-device-audio"
else
    echo "  [WARN]  evo.service ExecStart resolves to '${EXEC_START_RESOLVED:-<unset>}' (expected /opt/evo/bin/evo-device-audio)"
    echo "          (next \`systemctl restart evo\` will launch the wrong binary; check $SYSTEMD_DROPIN_DIR/exec-start.conf)"
fi

# MPD-restart sudoers drop-in present + the service user can
# dry-run the exact command.
if [[ -f "$SUDOERS_FILE" ]]; then
    if sudo -u "$SERVICE_USER" sudo -n -l -- "$SYSTEMCTL_BIN" restart mpd >/dev/null 2>&1; then
        echo "  [ok]    $SERVICE_USER permitted to run \`$SYSTEMCTL_BIN restart mpd\` via NOPASSWD"
    else
        echo "  [WARN]  sudo -n -l -- $SYSTEMCTL_BIN restart mpd did not match for $SERVICE_USER"
        echo "          (review $SUDOERS_FILE and Environment=EVO_SYSTEMCTL in $SYSTEMD_DROPIN_DIR/mpd-restart-privileges.conf)"
    fi
else
    echo "  [skip]  MPD-restart sudoers drop-in not installed"
fi

# network.nm sudoers drop-in present + the service user can
# dry-run the nmcli binary.
if [[ -f "$NETWORK_NM_SUDOERS_FILE" ]]; then
    if sudo -u "$SERVICE_USER" sudo -n -l -- "$NMCLI_BIN" >/dev/null 2>&1; then
        echo "  [ok]    $SERVICE_USER permitted to run \`$NMCLI_BIN\` via NOPASSWD"
    else
        echo "  [WARN]  sudo -n -l -- $NMCLI_BIN did not match for $SERVICE_USER"
        echo "          (review $NETWORK_NM_SUDOERS_FILE; ensure binary path matches plugin config nmcli_path)"
    fi
else
    echo "  [skip]  network.nm sudoers drop-in not installed"
fi

# hardware-audio sudoers drop-in present + dacs.json catalogue
# resolvable at the canonical share path.
if [[ -f "$HARDWARE_AUDIO_SUDOERS_FILE" ]]; then
    echo "  [ok]    hardware-audio sudoers drop-in installed at $HARDWARE_AUDIO_SUDOERS_FILE"
else
    echo "  [skip]  hardware-audio sudoers drop-in not installed"
fi
if [[ -f "$DACS_CATALOGUE_PATH" ]]; then
    echo "  [ok]    DAC catalogue installed at $DACS_CATALOGUE_PATH"
else
    echo "  [skip]  DAC catalogue not installed at $DACS_CATALOGUE_PATH (plugin reads its embedded copy)"
fi

# Fragment path writable by service user.
if [[ -w "$MPD_FRAGMENT_PATH" ]] && \
   [[ "$(stat -c '%U' "$MPD_FRAGMENT_PATH")" == "$SERVICE_USER" ]]; then
    echo "  [ok]    $MPD_FRAGMENT_PATH writable by $SERVICE_USER"
else
    echo "  [WARN]  $MPD_FRAGMENT_PATH not owned by $SERVICE_USER or not writable"
fi

# client_acl present (operator capability gate).
if [[ -f /etc/evo/client_acl.toml ]]; then
    echo "  [ok]    /etc/evo/client_acl.toml installed (plans_admin + plugins_admin + reconciliation_admin granted to matching-UID local peers)"
else
    echo "  [WARN]  /etc/evo/client_acl.toml absent — operator wire-ops (evo-plugin-tool plan / admin) will be refused until installed"
fi

# HTTPS substrate drop-in: the framework reference unit
# declares no HTTPS environment because the framework binary
# is transport-agnostic; the distribution layers an
# `https.conf` drop-in that wires EVO_HTTPS_LISTEN_ADDR +
# EVO_HTTPS_STATE_DIR + EVO_HTTPS_STATIC_DIR. Verify the
# drop-in is in place and the listen address resolves;
# without it the operator UI's browser cannot reach the
# device because there is no HTTPS listener bound.
if [[ -f "$SYSTEMD_DROPIN_DIR/https.conf" ]]; then
    HTTPS_LISTEN_RESOLVED="$("$SYSTEMCTL_BIN" show evo --no-pager -p Environment \
        2>/dev/null | tr ' ' '\n' | grep -oE 'EVO_HTTPS_LISTEN_ADDR=[^[:space:]]+' \
        | head -1 | cut -d= -f2)"
    if [[ -n "$HTTPS_LISTEN_RESOLVED" ]]; then
        echo "  [ok]    evo HTTPS drop-in installed; EVO_HTTPS_LISTEN_ADDR=$HTTPS_LISTEN_RESOLVED"
    else
        echo "  [WARN]  evo HTTPS drop-in present but EVO_HTTPS_LISTEN_ADDR not resolved from unit env"
        echo "          (review $SYSTEMD_DROPIN_DIR/https.conf)"
    fi
else
    echo "  [WARN]  evo HTTPS drop-in absent — operator browser cannot reach the device; bootstrap should install $SYSTEMD_DROPIN_DIR/https.conf"
fi

# Audio chain probe: confirm the rendered `ctl.evo` opens
# against the detected/operator-selected card via amixer.
# The control interface is the cheap probe — it opens the
# card's mixer (mirroring the path mpd's hardware mixer
# walks) without acquiring the playback PCM (which mpd may
# already hold post-restart). Failure here is the exact
# class of break the operator otherwise discovers later via
# mpd's `default detected output (sndio)` cascade — a
# misconfigured card name surfaces as an amixer open error.
if command -v amixer >/dev/null 2>&1; then
    PROBE_OUT=""
    if PROBE_OUT="$(amixer -D evo info 2>&1)"; then
        echo "  [ok]    ctl.evo opens against card '$AUDIO_CARD' (amixer probe)"
    else
        echo "  [WARN]  ctl.evo failed to open against card '$AUDIO_CARD'"
        echo "          (review $ASOUND_CONF_PATH; verify card name matches \`aplay -l\`)"
        echo "$PROBE_OUT" | head -5 | sed 's/^/          /'
    fi
else
    echo "  [skip]  amixer not available — ctl.evo probe skipped"
fi

# Active PCM playback-path probe: confirm `pcm.evo` opens for
# PLAYBACK against the substituted card. The amixer probe above
# proves only the control interface resolves; MPD's actual
# write path is the PCM, and MPD opens the device lazily on
# first audio_output use — so a misconfigured `pcm.evo` may
# pass the amixer probe and still surface later as
# `Failed to open ALSA device "evo": No such device` once the
# operator hits play. `aplay --dump-hw-params` opens the PCM,
# negotiates hardware parameters, dumps them, and exits
# WITHOUT writing audio frames — no audible playback during
# install. Exit 0 = PCM open + HW param negotiation succeeded.
# Probe input: prefer the distribution-shipped silent probe
# WAV (52-byte silent file shipped under dist/alsa/), fall
# back to the Debian alsa-utils-data Front_Center.wav. On a
# host with neither, skip with a clear WARN — the probe is the
# last line of defence against the placeholder-not-substituted
# class; the placeholder-residue check above is the first.
PROBE_WAV=""
if [[ -f "$DIST_DIR/alsa/silent-probe.wav" ]]; then
    PROBE_WAV="$DIST_DIR/alsa/silent-probe.wav"
elif [[ -f /usr/share/sounds/alsa/Front_Center.wav ]]; then
    PROBE_WAV="/usr/share/sounds/alsa/Front_Center.wav"
fi
if command -v aplay >/dev/null 2>&1 && [[ -n "$PROBE_WAV" ]]; then
    PROBE_OUT=""
    if PROBE_OUT="$(aplay -D evo --dump-hw-params "$PROBE_WAV" 2>&1)" \
        && printf '%s' "$PROBE_OUT" | grep -q '^HW Params of device "evo":'; then
        echo "  [ok]    pcm.evo opens for playback against card '$AUDIO_CARD' (aplay --dump-hw-params)"
    else
        echo "  [WARN]  pcm.evo failed to open for playback against card '$AUDIO_CARD'"
        echo "          (audio will fail at first play; review $ASOUND_CONF_PATH)"
        printf '%s\n' "$PROBE_OUT" | head -5 | sed 's/^/          /'
    fi
elif [[ -z "$PROBE_WAV" ]]; then
    echo "  [skip]  pcm.evo playback probe skipped — no probe WAV found"
    echo "          (install alsa-utils-data or ship dist/alsa/silent-probe.wav)"
else
    echo "  [skip]  pcm.evo playback probe skipped — aplay not available"
fi

# MPD audio_output probe: after the include + asound.conf are
# in place, mpd's `outputs` listing must show the
# evo-device-audio output (proves /etc/evo/mpd.conf's
# audio_output block is actually being read). Probe only when
# mpd is running; the asound.conf install step bounces mpd so
# this typically reads the freshly-loaded config.
if command -v mpc >/dev/null 2>&1 \
   && "$SYSTEMCTL_BIN" is-active mpd.service >/dev/null 2>&1; then
    if mpc outputs 2>/dev/null \
            | grep -q "evo-device-audio"; then
        echo "  [ok]    mpd reads $MPD_FRAGMENT_PATH (output 'evo-device-audio' listed)"
    else
        echo "  [WARN]  mpd does not list output 'evo-device-audio'"
        echo "          (verify $MPD_CONF_PATH includes $MPD_FRAGMENT_PATH; check 'mpc outputs')"
    fi
else
    echo "  [skip]  mpc/mpd not active — audio_output probe skipped"
fi

echo
echo "[bootstrap] complete. Next: systemctl restart evo.service"
