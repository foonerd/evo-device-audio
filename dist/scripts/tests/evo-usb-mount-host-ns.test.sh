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
assert_ok "version is 7" "[[ \"\$( \"\$WRAPPER\" --version )\" == \"evo-usb-mount 7\" ]]"
assert_ok "mount uses systemd-mount --collect" \
    "grep -qE -- 'systemd-mount --collect --fsck=no' \"\$WRAPPER\""
assert_ok "umount uses systemd-umount" \
    "grep -qE -- '/usr/bin/systemd-umount' \"\$WRAPPER\""
# Force eject must actually force. `-l` is a lazy detach to
# util-linux's umount and `--full` to systemd-umount, so the
# escalation has to reach `umount -l` / `fusermount -uz` as a
# child of PID 1. nsenter cannot: RestrictNamespaces=yes.
assert_ok "force detach never invokes systemd-umount -l" \
    "! grep -qE -- '^[[:space:]]*/usr/bin/systemd-umount -l' \"\$WRAPPER\""
assert_ok "force detach never nsenters (RestrictNamespaces blocks setns)" \
    "! grep -vE '^[[:space:]]*#' \"\$WRAPPER\" | grep -qE -- 'nsenter'"
assert_ok "force detach asks PID 1 via systemd-run" \
    "grep -qF -- '/usr/bin/systemd-run --quiet --wait --collect --pipe' \"\$WRAPPER\""
assert_ok "force detach lazily unmounts the target there" \
    "grep -qF -- '/bin/umount -i -l \"\${target}\"' \"\$WRAPPER\""
assert_ok "force detach uses fusermount -uz on fuseblk while MPD indexes" \
    "grep -qE -- 'fusermount3|-uz' \"\$WRAPPER\""
assert_ok "force detach tries a clean unmount before escalating" \
    "grep -qF -- 'if /usr/bin/systemd-umount \"\${target}\"' \"\$WRAPPER\""
# One EBUSY, two wordings: util-linux's and systemd's. The
# exit-4 contract in the wrapper's header only fires if both
# are matched.
assert_ok "EBUSY is matched on systemd's wording too" \
    "grep -qE -- 'device or resource busy' \"\$WRAPPER\""
assert_ok "EBUSY matching is case-folded" \
    "grep -qF -- 'umount_stderr,,' \"\$WRAPPER\""
assert_ok "mount action does not call raw mount -t" \
    "! grep -E '^[[:space:]]*mount -t ' \"\$WRAPPER\""
assert_ok "host truth is PID 1 mount table" \
    "grep -qE -- 'findmnt --task 1 --mountpoint' \"\$WRAPPER\""
assert_ok "NTFS attach type is ntfs-3g" \
    "grep -qE -- 'systemd_type=\"ntfs-3g\"' \"\$WRAPPER\""
assert_ok "NTFS refuses without the ntfs-3g helper" \
    "grep -qE -- 'ntfs-3g is required to mount NTFS' \"\$WRAPPER\""
assert_ok "mount success is checked in the host namespace" \
    "grep -qE -- 'host namespace does not show' \"\$WRAPPER\""

echo
echo "${PASS} passed, ${FAIL} failed"
[[ "${FAIL}" -eq 0 ]]
