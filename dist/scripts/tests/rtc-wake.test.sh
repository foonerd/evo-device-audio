#!/usr/bin/env bash
# rtc-wake.test.sh — regression test for the distribution RTC
# wake privilege path: the root-owned wrapper that writes the
# kernel wakealarm node, and the narrow sudoers fragment that
# grants the steward tenant that one command.
#
# The class this exists for: the steward runs as the install-time
# service user and is never root (HARD-LOCKS HL-001), so it
# cannot write /sys/class/rtc/rtc0/wakealarm itself. A unit test
# that drives a private helper against a tempfile attests
# nothing about that node — the tempfile is writable, the helper
# is not the production path, and the green it produces is a
# lie. There is no such helper any more. The production write
# path is `sudo -n /usr/local/bin/evo-rtc-wake <seconds>`, so
# the wrapper is what has to fail or succeed here.
#
# The wrapper under test is the shipped file with only its
# target path redirected at a fixture. The substitution is
# asserted before the copy is ever executed: if the shipped
# path drifts out of the sed pattern, this test stops rather
# than sending a write at the real RTC node.
#
# Invoke from anywhere:
#
#   bash dist/scripts/tests/rtc-wake.test.sh
#
# Exit codes:
#   0 — every case passed.
#   1 — at least one case failed, or a named gate could not run.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
WRAPPER="$DIST_DIR/bin/evo-rtc-wake"
SUDOERS="$DIST_DIR/sudoers.d/evo-rtc-wake.in"

# The tenant substituted into the fragment for the visudo gate.
# sudoers parses an unresolvable user name, so this deliberately
# does not have to exist on the host running the test.
readonly DUMMY_SERVICE_USER="evo-rtc-wake-test-tenant"

PASS=0
FAIL=0
SKIP=0

ok() {
    PASS=$((PASS + 1))
    echo "PASS  $1"
}

bad() {
    FAIL=$((FAIL + 1))
    echo "FAIL  $1" >&2
}

skip() {
    SKIP=$((SKIP + 1))
    echo "SKIP  $1" >&2
}

# Assert a command fails. The wrapper's contract is that a write
# it cannot perform is a non-zero exit, so every refusal case
# below is stated as "this must not succeed" rather than as an
# exit status compared to a specific number: bash reports a
# failed redirection as 1, and pinning that would test bash.
refuses() {
    local name="$1"; shift
    if "$@" >/dev/null 2>&1; then
        bad "$name (exited 0)"
    else
        ok "$name"
    fi
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# ----------------------------------------------------------
# Shipped-file shape. These guard the sequence and the grant,
# so a rewrite of either file that drops them fails here.
# ----------------------------------------------------------
if [[ -x "$WRAPPER" ]]; then
    ok "wrapper is executable"
else
    bad "wrapper is not executable: $WRAPPER"
fi

if grep -q 'RTC_WAKEALARM_PATH="/sys/class/rtc/rtc0/wakealarm"' "$WRAPPER"; then
    ok "wrapper targets the kernel wakealarm node"
else
    bad "wrapper does not target /sys/class/rtc/rtc0/wakealarm"
fi

if grep -q "'0' > \"\$RTC_WAKEALARM_PATH\"" "$WRAPPER"; then
    ok "wrapper clears the alarm before programming"
else
    bad "wrapper does not clear the alarm first"
fi

if grep -q '"\$programmed" > "$RTC_WAKEALARM_PATH"' "$WRAPPER"; then
    ok "wrapper programs the instant in the RTC's own time"
else
    bad "wrapper does not program the RTC-time value"
fi

if grep -q 'Cmnd_Alias EVO_RTC_WAKE = /usr/local/bin/evo-rtc-wake' "$SUDOERS"; then
    ok "sudoers names one enumerated command"
else
    bad "sudoers Cmnd_Alias is missing or not enumerated"
fi

if grep -q '@EVO_SERVICE_USER@ ALL=(ALL) NOPASSWD: EVO_RTC_WAKE' "$SUDOERS"; then
    ok "sudoers grants the substituted tenant that alias only"
else
    bad "sudoers grant line is missing or widened"
fi

# ----------------------------------------------------------
# The visudo gate. This is where the rendered fragment is
# proven parseable — bootstrap.sh runs the same check at
# install time, but an install-time check is not a package
# test, so the render happens here against a dummy tenant.
# ----------------------------------------------------------
VISUDO=""
if command -v visudo >/dev/null 2>&1; then
    VISUDO="$(command -v visudo)"
elif [[ -x /usr/sbin/visudo ]]; then
    # The tenant's PATH need not carry sbin.
    VISUDO=/usr/sbin/visudo
fi

if [[ -z "$VISUDO" ]]; then
    # Not a skip. This is the named gate for the row, and the
    # product cannot install its sudoers fragments without it;
    # reporting green here would be reporting a negative from a
    # tool that was never shown able to report a positive.
    bad "visudo not found — the sudoers gate could not run"
else
    # The shipped byte, not the rendered copy. This file ended on
    # `EVO_RTC_WAKE` with no newline after it; visudo rejects that
    # outright - "missing line terminator at end of file" - and
    # bootstrap runs the same check and exits instead of
    # installing, so the grant never landed. The visudo gate below
    # catches it today, but only because this host's visudo is
    # strict about it. Naming the byte says which one is missing
    # rather than only that the parse failed.
    if [[ -n "$(tail -c1 "$SUDOERS")" ]]; then
        bad "sudoers template does not end with a newline"
    else
        ok "sudoers template ends with a newline"
    fi

    rendered="$WORK/evo-rtc-wake"
    sed "s|@EVO_SERVICE_USER@|$DUMMY_SERVICE_USER|g" "$SUDOERS" > "$rendered"

    if grep -q '@EVO_SERVICE_USER@' "$rendered"; then
        bad "sudoers render left @EVO_SERVICE_USER@ unsubstituted"
    else
        ok "sudoers renders with the service user substituted"
    fi

    if "$VISUDO" -c -f "$rendered" >/dev/null 2>&1; then
        ok "rendered sudoers fragment passes visudo -c -f"
    else
        bad "rendered sudoers fragment fails visudo -c -f"
    fi

    # Control: visudo must be shown able to reject before its
    # acceptance above counts for anything.
    malformed="$WORK/malformed-sudoers"
    printf 'Cmnd_Alias = = =\nthis is not a sudoers file\n' > "$malformed"
    refuses "visudo -c -f rejects a malformed fragment" \
        "$VISUDO" -c -f "$malformed"
fi

# ----------------------------------------------------------
# The wrapper itself, against fixtures standing in for the kernel
# nodes and the system clock.
#
# The wrapper measures the RTC against the system clock, so each
# case fixes both: NOW is the system clock, `since_epoch` is the RTC
# read as UTC. A copy of the shipped file is made with the three
# paths and the clock redirected, and refuses to run if any real
# path is left in it.
# ----------------------------------------------------------
NOW=1795000000
fixture="$WORK/wakealarm"
since="$WORK/since_epoch"

make_wrapper() { # name wakealarm-path since-path
    local out="$WORK/$1"
    sed -e "s|/sys/class/rtc/rtc0/wakealarm|$2|" \
        -e "s|/sys/class/rtc/rtc0/since_epoch|$3|" \
        -e "s|^now_utc() { date -u +%s; }|now_utc() { echo $NOW; }|" \
        "$WRAPPER" > "$out"
    chmod 0755 "$out"
    if grep -q '/sys/class/rtc' "$out" || grep -q 'date -u +%s' "$out"; then
        bad "redirect failed for $1 — the copy still reaches the real node or clock"
        echo "summary: $PASS passed, $FAIL failed, $SKIP skipped" >&2
        exit 1
    fi
    echo "$out"
}

test_wrapper="$(make_wrapper evo-rtc-wake.fixture "$fixture" "$since")"
ok "fixture redirect replaced both kernel node paths and the clock"

# Run the wrapper against an RTC reading; print its stdout and exit code.
program() { # since_epoch target
    printf '%s' "$1" > "$since"
    set +e
    OUT="$("$test_wrapper" "$2" 2>"$WORK/stderr")"
    RC=$?
    set -e
}

expect_programmed() { # name since target expected-node expected-mode
    printf '%s' '1700000000' > "$fixture"
    program "$2" "$3"
    if [[ $RC -ne 0 ]]; then
        bad "$1: wrapper exited $RC ($(cat "$WORK/stderr"))"
    elif [[ "$(cat "$fixture")" != "$4" ]]; then
        bad "$1: node holds $(cat "$fixture"), wanted $4"
    elif [[ "$OUT" != *"rtc=$5"* ]]; then
        bad "$1: said '$OUT', wanted rtc=$5"
    else
        ok "$1"
    fi
}

expect_refused() { # name since target exit-code
    printf '%s' '1700000000' > "$fixture"
    program "$2" "$3"
    if [[ $RC -ne $4 ]]; then
        bad "$1: exited $RC, wanted $4"
    elif [[ "$(cat "$fixture")" != 1700000000 ]]; then
        bad "$1: the node was touched ($(cat "$fixture"))"
    else
        ok "$1"
    fi
}

TARGET=$((NOW + 36000))

# An RTC holding UTC: programmed as given.
expect_programmed "an RTC holding UTC is programmed with the UTC instant" \
    "$NOW" "$TARGET" "$TARGET" utc
# Drift is not a zone: a minute fast or slow is left alone.
expect_programmed "an RTC a minute fast is still UTC" \
    "$((NOW + 60))" "$TARGET" "$TARGET" utc
expect_programmed "an RTC a minute and a half slow is still UTC" \
    "$((NOW - 90))" "$TARGET" "$TARGET" utc
# An RTC holding local time: the offset is added.
expect_programmed "an RTC on local time one hour east gets the instant plus one hour" \
    "$((NOW + 3600))" "$TARGET" "$((TARGET + 3600))" local
expect_programmed "an RTC on local time five hours west gets the instant minus five hours" \
    "$((NOW - 18000))" "$TARGET" "$((TARGET - 18000))" local
expect_programmed "a half-hour zone (+5:30) is recognised" \
    "$((NOW + 19800))" "$TARGET" "$((TARGET + 19800))" local
expect_programmed "the smallest real offset, half an hour, is recognised" \
    "$((NOW + 1800))" "$TARGET" "$((TARGET + 1800))" local
expect_refused "a quarter hour is not a zone: refused" \
    "$((NOW + 900))" "$TARGET" 3
expect_programmed "a 45-minute zone (+5:45) is recognised" \
    "$((NOW + 20700))" "$TARGET" "$((TARGET + 20700))" local
expect_programmed "an RTC on local time with a little drift on top is still the zone" \
    "$((NOW + 3600 + 50))" "$TARGET" "$((TARGET + 3600))" local
# The offset is the RTC's own value now, not the zone's value at the wake.
expect_programmed "the offset used is the RTC's present one, whatever the wake's date" \
    "$((NOW + 3600))" "$((NOW + 86400 * 200))" "$((NOW + 86400 * 200 + 3600))" local

# Cannot be told: refused, node untouched.
expect_refused "an RTC 1000 s away is neither drift nor a zone: refused" \
    "$((NOW + 1000))" "$TARGET" 3
expect_refused "an RTC half a day and a bit fast is refused" \
    "$((NOW + 51000))" "$TARGET" 3
expect_refused "an RTC more than fourteen hours behind is refused" \
    "$((NOW - 90000))" "$TARGET" 3
expect_refused "an RTC whose reading is not a number is refused" \
    "not-a-number" "$TARGET" 3
rm -f "$since"
printf '%s' '1700000000' > "$fixture"
if "$test_wrapper" "$TARGET" >/dev/null 2>&1; then bad "an unreadable RTC time is refused (exited 0)"
elif [[ "$(cat "$fixture")" == 1700000000 ]]; then ok "an unreadable RTC time is refused and the node is untouched"
else bad "an unreadable RTC time touched the node"; fi

# A wake that the RTC's epoch cannot hold.
expect_refused "a wake before the RTC's epoch (offset pushes it below zero) is refused" \
    "$((NOW - 18000))" 100 2

# Clearing never needs the RTC.
printf '%s' '1700000000' > "$fixture"
printf '%s' 'garbage' > "$since"
if ! "$test_wrapper" 0 >/dev/null 2>&1; then
    bad "clearing the alarm: wrapper exited non-zero"
elif [[ "$(cat "$fixture")" == 0 ]]; then
    ok "zero clears the alarm without reading the RTC"
else
    bad "clear left $(cat "$fixture") in the wakealarm node"
fi

# Replace: a live alarm is overwritten.
printf '%s' "$NOW" > "$since"
printf '%s' '1700000000' > "$fixture"
if ! "$test_wrapper" 1700000123 >/dev/null 2>&1; then
    bad "replacing a live alarm: wrapper exited non-zero"
elif [[ "$(cat "$fixture")" == 1700000123 ]]; then
    ok "replacing a live alarm programs the new value"
else
    bad "replace left $(cat "$fixture") in the wakealarm node"
fi

# Order of the writes: the alarm is cleared, then programmed, so a
# replacement never leaves the previous wake armed. The copy appends each
# write to a history instead of truncating a node, so the sequence shows.
hist="$WORK/write-history"
ordered="$WORK/evo-rtc-wake.ordered"
sed -e "s|/sys/class/rtc/rtc0/since_epoch|$since|" \
    -e "s|^now_utc() { date -u +%s; }|now_utc() { echo $NOW; }|" \
    -e "s|printf '%s' '0' > \"\$RTC_WAKEALARM_PATH\"|echo 0 >> \"$hist\"|" \
    -e "s|printf '%s' \"\$programmed\" > \"\$RTC_WAKEALARM_PATH\"|echo \"\$programmed\" >> \"$hist\"|" \
    -e "s|^readback=\"\$(cat \"\$RTC_WAKEALARM_PATH\")\"|readback=\"\$(tail -1 \"$hist\")\"|" \
    "$WRAPPER" > "$ordered"
chmod 0755 "$ordered"
printf '%s' "$NOW" > "$since"; : > "$hist"
if grep -q 'RTC_WAKEALARM_PATH"$' "$ordered" && grep -q '> "\$RTC_WAKEALARM_PATH"' "$ordered"; then
    bad "write-order redirect failed — the copy still writes the node"
else
    "$ordered" "$TARGET" >/dev/null 2>&1 || true
    if [[ "$(tr '\n' ' ' < "$hist")" == "0 $TARGET " ]]; then
        ok "a program clears the previous alarm first, then writes the new one"
    else
        bad "write order was: $(tr '\n' ' ' < "$hist")"
    fi
fi

# The node must keep what was written: a read-back that differs fails.
lying="$WORK/evo-rtc-wake.lying"
sed -e "s|/sys/class/rtc/rtc0/wakealarm|$fixture|" \
    -e "s|/sys/class/rtc/rtc0/since_epoch|$since|" \
    -e "s|^now_utc() { date -u +%s; }|now_utc() { echo $NOW; }|" \
    -e 's|^readback="\$(cat "\$RTC_WAKEALARM_PATH")"|readback="1"|' "$WRAPPER" > "$lying"
chmod 0755 "$lying"
if ! grep -q '^readback="1"' "$lying"; then
    bad "read-back redirect failed"
else
    set +e; "$lying" "$TARGET" >/dev/null 2>&1; rc=$?; set -e
    if [[ $rc -eq 4 ]]; then ok "a node that does not hold the written value is exit 4"; else bad "a lying read-back exited $rc, wanted 4"; fi
fi

# Argument validation: the wrapper is the privileged target, so
# anything that is not epoch seconds is refused before a write.
refuses "non-numeric argument is refused" "$test_wrapper" invalid
refuses "a negative argument is refused" "$test_wrapper" -5
refuses "an argument too long to be a clock is refused" "$test_wrapper" 1234567890123
refuses "no argument is refused" "$test_wrapper"
refuses "two arguments are refused" "$test_wrapper" 1 2

# Unwritable node: a read-only wakealarm is a failed program,
# not a silent success. Root ignores 0444, which would make this
# case report the wrapper broken when it is not — so it is
# skipped rather than inverted, and the summary says so.
printf '%s' "$NOW" > "$since"
printf '%s' '1700000000' > "$fixture"
chmod 0444 "$fixture"
if [[ "$EUID" -eq 0 ]]; then
    skip "read-only wakealarm is a failed program (running as root; 0444 does not deny root)"
else
    refuses "read-only wakealarm is a failed program" \
        "$test_wrapper" 1700000123
fi
chmod 0644 "$fixture"

# Absent rtc0: the parent directory does not exist. This is the
# no-RTC host, and it is a failure, not a no-op — a machine with
# no wakealarm node cannot honour a must-wake appointment and
# must not report that it did.
absent_wrapper="$(make_wrapper evo-rtc-wake.absent "$WORK/no-rtc-class/rtc0/wakealarm" "$WORK/no-rtc-class/rtc0/since_epoch")"
refuses "absent rtc0 is a failed program" "$absent_wrapper" 1700000123
refuses "absent rtc0 cannot be cleared into a false success" "$absent_wrapper" 0

echo "summary: $PASS passed, $FAIL failed, $SKIP skipped"
if [[ $FAIL -eq 0 ]]; then
    echo "rtc-wake.test.sh: passed"
else
    exit 1
fi
