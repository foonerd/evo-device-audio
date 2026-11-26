#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Just a Nerd
#
# install-updater.sh — place the updater helper on a box.
#
# The helper cannot deliver itself: a helper that rewrote its own trust or
# allow list would have no root of its own. So this script, run as root by
# the installer (bootstrap.sh) or by hand on a fielded box, places:
#
#   /usr/local/bin/evo-apply-update                          the sudo target
#   /usr/libexec/evo-apply-update/evo-apply-update-worker    the unit's process
#   /etc/systemd/system/evo-apply-update@.service
#   /etc/evo-updater/{updater.toml,allow.toml,trust.d/}
#   /var/lib/evo-updater/
#   /etc/sudoers.d/evo-apply-update
#
# With --fix-opt-ownership it also makes /opt/evo, /opt/evo/bin and
# /opt/evo/plugins root-owned. A fresh install already does; a box that
# drifted (service-user-owned) is refused by the worker until this runs.
#
# Usage (root):
#   install-updater.sh --dist <dist dir> --bin-dir <dir holding the two
#       binaries> --service-user <user> [--fix-opt-ownership]
#
# EVO_INSTALL_ROOT=<dir> prefixes every path (tests); then neither root nor
# systemd is required and ownership is left to the caller.

set -euo pipefail

DIST_DIR=""
BIN_DIR=""
SERVICE_USER=""
FIX_OPT=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dist) DIST_DIR="$2"; shift 2 ;;
        --bin-dir) BIN_DIR="$2"; shift 2 ;;
        --service-user) SERVICE_USER="$2"; shift 2 ;;
        --fix-opt-ownership) FIX_OPT=1; shift ;;
        *) echo "FAIL: unknown argument: $1" >&2; exit 1 ;;
    esac
done
[[ -n "$DIST_DIR" && -n "$BIN_DIR" && -n "$SERVICE_USER" ]] || {
    echo "FAIL: --dist, --bin-dir and --service-user are required" >&2; exit 1; }
[[ "$SERVICE_USER" =~ ^[a-z_][a-z0-9_-]{0,31}$ ]] || {
    echo "FAIL: service user '$SERVICE_USER' is not a plain account name" >&2; exit 1; }

ROOT="${EVO_INSTALL_ROOT:-}"
if [[ -z "$ROOT" && "$(id -u)" -ne 0 ]]; then
    echo "FAIL: must run as root" >&2
    exit 1
fi
# root:root unless a test prefix is in use.
OWN=(-o root -g root)
[[ -n "$ROOT" ]] && OWN=()

for f in "$BIN_DIR/evo-apply-update" "$BIN_DIR/evo-apply-update-worker" \
         "$DIST_DIR/systemd/evo-apply-update@.service" \
         "$DIST_DIR/etc-updater/allow.toml" "$DIST_DIR/etc-updater/updater.toml" \
         "$DIST_DIR/sudoers.d/evo-apply-update.in" \
         "$DIST_DIR/keys/commons-plugin-signing-public.pem" \
         "$DIST_DIR/keys/commons-plugin-signing-public.meta.toml"; do
    [[ -f "$f" ]] || { echo "FAIL: missing $f" >&2; exit 2; }
done

install -d -m 0755 "${OWN[@]}" \
    "$ROOT/usr/local/bin" "$ROOT/usr/libexec/evo-apply-update" \
    "$ROOT/etc/systemd/system" "$ROOT/etc/sudoers.d" \
    "$ROOT/etc/evo-updater" "$ROOT/etc/evo-updater/trust.d" \
    "$ROOT/var/lib/evo-updater" "$ROOT/opt/evo/rollback"

install -m 0755 "${OWN[@]}" "$BIN_DIR/evo-apply-update" "$ROOT/usr/local/bin/evo-apply-update"
install -m 0755 "${OWN[@]}" "$BIN_DIR/evo-apply-update-worker" \
    "$ROOT/usr/libexec/evo-apply-update/evo-apply-update-worker"
install -m 0644 "${OWN[@]}" "$DIST_DIR/systemd/evo-apply-update@.service" \
    "$ROOT/etc/systemd/system/evo-apply-update@.service"

# The allow list is the installer's: always replaced. updater.toml is the
# operator's once placed: kept. The commons key is placed or refreshed; any
# other key already in trust.d (a lab key, by hand) is never touched.
install -m 0644 "${OWN[@]}" "$DIST_DIR/etc-updater/allow.toml" "$ROOT/etc/evo-updater/allow.toml"
if [[ ! -f "$ROOT/etc/evo-updater/updater.toml" ]]; then
    install -m 0644 "${OWN[@]}" "$DIST_DIR/etc-updater/updater.toml" "$ROOT/etc/evo-updater/updater.toml"
fi
install -m 0644 "${OWN[@]}" "$DIST_DIR/keys/commons-plugin-signing-public.pem" \
    "$ROOT/etc/evo-updater/trust.d/commons-plugin-signing-public.pem"
install -m 0644 "${OWN[@]}" "$DIST_DIR/keys/commons-plugin-signing-public.meta.toml" \
    "$ROOT/etc/evo-updater/trust.d/commons-plugin-signing-public.meta.toml"

TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT
sed -e "s|@EVO_SERVICE_USER@|$SERVICE_USER|g" "$DIST_DIR/sudoers.d/evo-apply-update.in" > "$TMP"
if [[ -z "$ROOT" ]]; then
    visudo -c -f "$TMP" >/dev/null || { echo "FAIL: updater sudoers failed visudo -c" >&2; exit 2; }
fi
install -m 0440 "${OWN[@]}" "$TMP" "$ROOT/etc/sudoers.d/evo-apply-update"

if [[ "$FIX_OPT" -eq 1 && -z "$ROOT" ]]; then
    for d in /opt/evo /opt/evo/bin /opt/evo/plugins; do
        [[ -d "$d" ]] && { chown root:root "$d"; chmod 0755 "$d"; }
    done
fi
if [[ -z "$ROOT" ]] && command -v systemctl >/dev/null 2>&1 && [[ -d /run/systemd/system ]]; then
    systemctl daemon-reload
fi
echo "[install-updater] placed the updater helper (service user: $SERVICE_USER)"
