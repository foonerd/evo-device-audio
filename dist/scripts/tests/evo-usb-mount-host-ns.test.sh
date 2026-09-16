#!/usr/bin/env bash
# evo-usb-mount-host-ns.test.sh — USB mount must land where mpd
# and the operator can see it.
#
# ProtectSystem=strict gives the steward its own mount namespace.
# Raw mount(8) from the wrapper stays in that namespace: the
# helper returns 0, the leaf directory exists, and the volume's
# files are invisible on the host. systemd-mount --collect asks
# PID 1 to attach the volume in the host namespace (same as
# network.shares).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WRAPPER="$(cd "$SCRIPT_DIR/../../../plugins/org.evoframework.storage.usb/dist/bin" && pwd)/evo-usb-mount"

PASS=0
FAIL=0

assert_ok() {
    local name="$1"
    if eval "$2"; then
        echo "PASS  $name"
        PASS=$((PASS + 1))
    else
        echo "FAIL  $name"
        FAIL=$((FAIL + 1))
    fi
}

assert_ok "wrapper is present" "[[ -f \"\$WRAPPER\" ]]"
assert_ok "version is 4" "[[ \"\$( \"\$WRAPPER\" --version )\" == \"evo-usb-mount 4\" ]]"
assert_ok "mount uses systemd-mount --collect" \
    "grep -qE -- 'systemd-mount --collect --fsck=no' \"\$WRAPPER\""
assert_ok "umount uses systemd-umount" \
    "grep -qE -- '/usr/bin/systemd-umount' \"\$WRAPPER\""
assert_ok "force detach uses systemd-umount -l" \
    "grep -qE -- 'systemd-umount -l' \"\$WRAPPER\""
assert_ok "mount action does not call raw mount -t" \
    "! grep -E '^[[:space:]]*mount -t ' \"\$WRAPPER\""

echo
echo "${PASS} passed, ${FAIL} failed"
[[ "${FAIL}" -eq 0 ]]
