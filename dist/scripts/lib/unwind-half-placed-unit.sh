# shellcheck shell=bash
#
# Pure decision: should the half-placed evo.service be taken
# back out, and do it if so. Returns 0 when the unit was
# removed (the caller then reloads systemd and tells the
# operator), 1 when there was nothing to unwind. Touches only
# the path it is given, so the regression tests under
# dist/scripts/tests/ can drive it against a temp root instead
# of the real /etc.
#
# Why this exists:
#
# evo-install.sh places /etc/systemd/system/evo.service (the
# framework reference unit) immediately before invoking
# bootstrap.sh. That unit carries no concrete ExecStart on
# purpose — the distribution's exec-start.conf drop-in supplies
# it, and bootstrap.sh is what writes that drop-in. So a
# bootstrap abort between the two leaves a unit systemd refuses
# to load, and every later `systemctl` on the box answers
# bad-setting instead of the real reason the install stopped.
# The operator is then debugging the wrong thing.
#
# The drop-in is the discriminator. Absent means this run got
# half way and the unit is ours to remove. Present means the
# pair is complete — a re-install over a working device is the
# common case — and it must survive a failure here untouched.

unwind_half_placed_unit() {
    local root="${1:-}"
    local unit="${root}/etc/systemd/system/evo.service"
    local dropin="${root}/etc/systemd/system/evo.service.d/exec-start.conf"

    # Complete pair: leave it alone.
    [[ -f "$dropin" ]] && return 1
    # Nothing placed: nothing to unwind.
    [[ -f "$unit" ]] || return 1

    rm -f "$unit"
    return 0
}
