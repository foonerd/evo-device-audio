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
#   EVO_INSTALL_SYSTEMD_DROP_INS=0     — skip /etc/systemd/system/evo.service.d/
#   EVO_INSTALL_CLIENT_ACL=0           — skip /etc/evo/client_acl.toml install
#   EVO_INSTALL_MPD_FRAGMENT=0         — skip /etc/evo/mpd.conf bootstrap (empty file)
#   EVO_INSTALL_ASOUND_CONF=0          — skip /etc/asound.conf install
#   EVO_INSTALL_CATALOGUE=0            — skip /opt/evo/catalogue/default.toml install
#   EVO_INSTALL_MPD_INCLUDE=0          — skip injecting include of /etc/evo/mpd.conf
#                                       into /etc/mpd.conf
#   EVO_AUDIO_CARD=<name>              — override auto-detected ALSA card name
#                                       (env-var form; also available as --card)
#
# Per-step toggles let operators disable individual install
# legs without editing this script — useful when a vendor
# distribution composes its own privileged-action surface
# alongside the reference one.

set -euo pipefail

# Resolve the script's own directory so dist/* paths resolve
# regardless of the operator's CWD.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Defaults.
SERVICE_USER=""
SYSTEMCTL_BIN="/usr/bin/systemctl"
SUDOERS_FILE="/etc/sudoers.d/evo-mpd-restart"
NETWORK_NM_SUDOERS_FILE="/etc/sudoers.d/evo-network-nm"
NMCLI_BIN="/usr/bin/nmcli"
SYSTEMD_DROPIN_DIR="/etc/systemd/system/evo.service.d"
MPD_FRAGMENT_PATH="/etc/evo/mpd.conf"
MPD_CONF_PATH="/etc/mpd.conf"
ASOUND_CONF_PATH="/etc/asound.conf"
SKIP_SYSTEMD=0
AUDIO_CARD="${EVO_AUDIO_CARD:-}"

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
        -h|--help)
            grep -E '^# ' "$0" | sed 's/^# //'
            exit 0
            ;;
        *)
            echo "unknown argument: $1" >&2
            echo "usage: $0 [--service-user <name>] [--card <NAME>] [--skip-systemd]" >&2
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
# `aplay -l`. Refuse the install with an operator-readable
# error when no playback card is available (e.g. headless
# container, audio kernel modules absent). Reference
# distribution uses the I-Sabre Q2M card (name `DAC`); every
# other deployment substitutes its detected card.
# ----------------------------------------------------------
if [[ -z "$AUDIO_CARD" ]]; then
    if ! command -v aplay >/dev/null 2>&1; then
        echo "aplay not found on PATH; install alsa-utils or pass --card <NAME>" >&2
        exit 1
    fi
    # `aplay -l` prints lines like:
    #   card 0: I82801AAICH [Intel 82801AA-ICH], device 0: Intel ICH [Intel 82801AA-ICH]
    # The card NAME (kernel-stable, hot-plug-stable) sits
    # between `card N: ` and the next `[`. Prefer non-HDMI
    # cards (external DAC / USB / on-board analog) over HDMI
    # outputs — Pi-class boards enumerate HDMI before the
    # attached DAC, and the operator's intent for a music
    # appliance is the DAC, not the display's speakers.
    # Operators with HDMI-as-intended-output (e.g. AVR via
    # HDMI) override with --card.
    AUDIO_CARD="$(aplay -l 2>/dev/null | awk -F'[: ]+' '
        /^card [0-9]+/ {
            name = $3
            if (name !~ /^vc4hdmi/ && name !~ /HDMI/i) {
                print name
                exit
            }
        }
    ')"
    # Fall back to the first card when only HDMI cards are
    # available (HDMI display with speakers is a valid music
    # appliance target).
    if [[ -z "$AUDIO_CARD" ]]; then
        AUDIO_CARD="$(aplay -l 2>/dev/null \
            | awk -F'[: ]+' '/^card [0-9]+/ { print $3; exit }')"
    fi
    if [[ -z "$AUDIO_CARD" ]]; then
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
# Step 2: systemd drop-ins for the steward unit
# ----------------------------------------------------------
if [[ "${EVO_INSTALL_SYSTEMD_DROP_INS:-1}" != "0" && "$SKIP_SYSTEMD" == "0" ]]; then
    install -d -m 0755 "$SYSTEMD_DROPIN_DIR"
    install -m 0644 -o root -g root \
        "$DIST_DIR/systemd/evo.service.d/state-dir-mode.conf" \
        "$SYSTEMD_DROPIN_DIR/state-dir-mode.conf"
    echo "[bootstrap] installed $SYSTEMD_DROPIN_DIR/state-dir-mode.conf"

    install -m 0644 -o root -g root \
        "$DIST_DIR/systemd/evo.service.d/mpd-restart-privileges.conf" \
        "$SYSTEMD_DROPIN_DIR/mpd-restart-privileges.conf"
    echo "[bootstrap] installed $SYSTEMD_DROPIN_DIR/mpd-restart-privileges.conf"

    "$SYSTEMCTL_BIN" daemon-reload
    echo "[bootstrap] systemctl daemon-reload"
else
    echo "[bootstrap] systemd drop-ins skipped (EVO_INSTALL_SYSTEMD_DROP_INS=0 or --skip-systemd)"
fi

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
# ----------------------------------------------------------
if [[ "${EVO_INSTALL_ASOUND_CONF:-1}" != "0" ]]; then
    ASOUND_TEMPLATE="$DIST_DIR/alsa/asound.conf"
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
else
    echo "[bootstrap] EVO_INSTALL_ASOUND_CONF=0 — skipping asound.conf"
fi

# ----------------------------------------------------------
# Verification: confirm what we just installed.
# ----------------------------------------------------------
echo
echo "[verify] preflight checks:"

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
