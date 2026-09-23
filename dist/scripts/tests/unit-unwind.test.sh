#!/usr/bin/env bash
# unit-unwind.test.sh — regression test for the half-placed
# evo.service unwind in dist/scripts/lib/unwind-half-placed-unit.sh.
#
# Guards the class where evo-install.sh places the framework
# reference unit, bootstrap.sh then aborts before writing the
# exec-start.conf drop-in, and the box is left with a unit
# systemd refuses to load. Every later `systemctl` reports
# bad-setting and the operator debugs that instead of the
# install error that actually stopped them.
#
# The drop-in is the discriminator, so the two directions both
# matter: unwind when it is absent, keep hands off when it is
# present (a re-install over a working device).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIB_PATH="$(cd "$SCRIPT_DIR/../lib" && pwd)/unwind-half-placed-unit.sh"

# shellcheck source=../lib/unwind-half-placed-unit.sh
. "$LIB_PATH"

PASS=0
FAIL=0

# place_root <dir> <unit?> <dropin?> — build a temp /etc shape.
place_root() {
    local root="$1" want_unit="$2" want_dropin="$3"
    mkdir -p "$root/etc/systemd/system/evo.service.d"
    [[ "$want_unit" == "yes" ]] \
        && printf '[Unit]\nDescription=Evo steward\n' \
            > "$root/etc/systemd/system/evo.service"
    [[ "$want_dropin" == "yes" ]] \
        && printf '[Service]\nExecStart=\nExecStart=/opt/evo/bin/evo-device-audio\n' \
            > "$root/etc/systemd/system/evo.service.d/exec-start.conf"
    return 0
}

assert_unwind() {
    local name="$1" want_unit="$2" want_dropin="$3"
    local expect_rc="$4" expect_unit_after="$5"
    local root got_rc=0 unit_after
    root="$(mktemp -d)"
    place_root "$root" "$want_unit" "$want_dropin"
    unwind_half_placed_unit "$root" || got_rc=$?
    if [[ -f "$root/etc/systemd/system/evo.service" ]]; then
        unit_after="present"
    else
        unit_after="absent"
    fi
    rm -rf "$root"
    if [[ "$got_rc" == "$expect_rc" && "$unit_after" == "$expect_unit_after" ]]; then
        echo "PASS  $name (rc=$got_rc, unit=$unit_after)"
        PASS=$((PASS + 1))
    else
        echo "FAIL  $name (expected rc=$expect_rc unit=$expect_unit_after, \
got rc=$got_rc unit=$unit_after)"
        FAIL=$((FAIL + 1))
    fi
}

# The failure this exists for: fresh install, bootstrap aborted
# on card detection before the drop-in was written.
assert_unwind "fresh install, abort before drop-in → unit removed" \
    yes no 0 absent

# Re-install over a working device: the pair is complete and a
# failure here must not disarm a device that was fine.
assert_unwind "re-install, drop-in present → unit kept" \
    yes yes 1 present

# Bootstrap aborted before the unit was placed at all.
assert_unwind "nothing placed → nothing to unwind" \
    no no 1 absent

# Drop-in without a unit: not a shape the installer produces,
# but the function must not claim an unwind it did not do.
assert_unwind "drop-in only, no unit → no claim" \
    no yes 1 absent

echo ""
echo "unit-unwind.test.sh: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
