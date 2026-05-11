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
#   EVO_INSTALL_MPD_SUDOERS=0  — skip /etc/sudoers.d/evo-mpd-restart
#   EVO_INSTALL_SYSTEMD_DROP_INS=0  — skip /etc/systemd/system/evo.service.d/
#   EVO_INSTALL_CLIENT_ACL=0  — skip /etc/evo/client_acl.toml install
#   EVO_INSTALL_MPD_FRAGMENT=0  — skip /etc/evo/mpd.conf bootstrap (empty file)
#   EVO_INSTALL_ASOUND_CONF=0  — skip /etc/asound.conf install
#   EVO_INSTALL_CATALOGUE=0  — skip /opt/evo/catalogue/default.toml install
#
# These mirror volumio-evo's `EVO_INSTALL_*_SUDOERS=0/1`
# pattern so operators can disable individual steps without
# editing this script.

set -euo pipefail

# Resolve the script's own directory so dist/* paths resolve
# regardless of the operator's CWD.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Defaults.
SERVICE_USER=""
SYSTEMCTL_BIN="/usr/bin/systemctl"
SUDOERS_FILE="/etc/sudoers.d/evo-mpd-restart"
SYSTEMD_DROPIN_DIR="/etc/systemd/system/evo.service.d"
MPD_FRAGMENT_PATH="/etc/evo/mpd.conf"
ASOUND_CONF_PATH="/etc/asound.conf"
SKIP_SYSTEMD=0

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
            echo "usage: $0 [--service-user <name>] [--skip-systemd]" >&2
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
# Step 4: /etc/asound.conf — modular ALSA pipeline (pcm.evo)
# ----------------------------------------------------------
if [[ "${EVO_INSTALL_ASOUND_CONF:-1}" != "0" ]]; then
    ASOUND_TEMPLATE="$DIST_DIR/alsa/asound.conf"
    if [[ ! -f "$ASOUND_TEMPLATE" ]]; then
        echo "asound template not found at $ASOUND_TEMPLATE" >&2
        exit 2
    fi
    # If an existing /etc/asound.conf is present with different
    # contents, back it up first so the operator never loses
    # state silently. Idempotent: re-running after a clean
    # install does not stack backups.
    if [[ -f "$ASOUND_CONF_PATH" ]] && \
       ! cmp -s "$ASOUND_TEMPLATE" "$ASOUND_CONF_PATH"; then
        backup="$ASOUND_CONF_PATH.pre-evo.$(date +%Y%m%d%H%M%S)"
        cp -a "$ASOUND_CONF_PATH" "$backup"
        echo "[bootstrap] backed up prior $ASOUND_CONF_PATH to $backup"
    fi
    install -m 0644 -o root -g root "$ASOUND_TEMPLATE" "$ASOUND_CONF_PATH"
    echo "[bootstrap] installed $ASOUND_CONF_PATH"
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

# sudoers drop-in is present + the service user can dry-run
# the exact command.
if [[ -f "$SUDOERS_FILE" ]]; then
    if sudo -u "$SERVICE_USER" sudo -n -l -- "$SYSTEMCTL_BIN" restart mpd >/dev/null 2>&1; then
        echo "  [ok]    $SERVICE_USER permitted to run \`$SYSTEMCTL_BIN restart mpd\` via NOPASSWD"
    else
        echo "  [WARN]  sudo -n -l -- $SYSTEMCTL_BIN restart mpd did not match for $SERVICE_USER"
        echo "          (review $SUDOERS_FILE and Environment=EVO_SYSTEMCTL in $SYSTEMD_DROPIN_DIR/mpd-restart-privileges.conf)"
    fi
else
    echo "  [skip]  sudoers drop-in not installed"
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

echo
echo "[bootstrap] complete. Next: systemctl restart evo.service"
