#!/usr/bin/env bash
# state-dir-ownership.test.sh — /var/lib/evo must match the
# unit before evo.service starts. ProtectSystem=strict makes
# a StateDirectory ownership or mode fix EROFS (238).
#
# Bootstrap must not chmod 0750 (drop-in is 0755) and must
# not reparent the state dir to root after handing it to
# the tenant.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOOTSTRAP="$(cd "$SCRIPT_DIR/.." && pwd)/bootstrap.sh"
DROPIN="$(cd "$SCRIPT_DIR/../.." && pwd)/systemd/evo.service.d/state-dir-mode.conf"

PASS=0
FAIL=0
pass() { echo "PASS  $*"; PASS=$((PASS + 1)); }
fail() { echo "FAIL  $*"; FAIL=$((FAIL + 1)); }

if grep -qE '^StateDirectoryMode=0755$' "$DROPIN"; then
    pass "drop-in declares StateDirectoryMode=0755"
else
    fail "state-dir-mode.conf is not 0755"
fi

if grep -q 'chmod 0755 /var/lib/evo' "$BOOTSTRAP" \
    && ! grep -q 'chmod 0750 /var/lib/evo' "$BOOTSTRAP"; then
    pass "bootstrap chmods /var/lib/evo 0755, not 0750"
else
    fail "bootstrap still chmods /var/lib/evo 0750 or omits 0755"
fi

if grep -qE 'install -d -m 0755 -o root -g root /var/lib/evo$' "$BOOTSTRAP"; then
    fail "bootstrap still reparents /var/lib/evo to root"
else
    pass "bootstrap does not install -d -o root /var/lib/evo"
fi

if grep -qE 'install -d -m 0755 -o "\$SERVICE_USER" -g "\$SERVICE_USER" /var/lib/evo' \
        "$BOOTSTRAP"; then
    pass "music step creates /var/lib/evo as the tenant"
else
    fail "music step does not tenant-own /var/lib/evo"
fi

echo "summary: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
