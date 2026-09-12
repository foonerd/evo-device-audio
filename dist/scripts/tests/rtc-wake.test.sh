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

if grep -q '"\$1" > "$RTC_WAKEALARM_PATH"' "$WRAPPER"; then
    ok "wrapper programs the requested seconds"
else
    bad "wrapper does not program the requested seconds"
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
# The wrapper itself, against a fixture standing in for the
# kernel node.
# ----------------------------------------------------------
fixture="$WORK/wakealarm"
test_wrapper="$WORK/evo-rtc-wake.fixture"
sed "s|/sys/class/rtc/rtc0/wakealarm|$fixture|" "$WRAPPER" > "$test_wrapper"
chmod 0755 "$test_wrapper"

# Refuse to run a copy that still points at the real node.
if grep -q '/sys/class/rtc' "$test_wrapper"; then
    bad "fixture redirect failed — copy still targets the real RTC node"
    echo "summary: $PASS passed, $FAIL failed, $SKIP skipped" >&2
    exit 1
fi
ok "fixture redirect replaced the kernel node path"

# Replace: a live alarm is overwritten with the new seconds.
#
# The invocation is guarded rather than bare: a wrapper broken
# badly enough to exit non-zero here would otherwise take the
# whole file down under `set -e`, and a run that dies before its
# summary reports nothing about the cases after this one.
printf '%s' '1700000000' > "$fixture"
if ! "$test_wrapper" 1700000123 >/dev/null 2>&1; then
    bad "replacing a live alarm: wrapper exited non-zero"
elif [[ "$(cat "$fixture")" == 1700000123 ]]; then
    ok "replacing a live alarm programs the new UTC epoch seconds"
else
    bad "replace left $(cat "$fixture") in the wakealarm node"
fi

# Clear: zero disarms.
if ! "$test_wrapper" 0 >/dev/null 2>&1; then
    bad "clearing the alarm: wrapper exited non-zero"
elif [[ "$(cat "$fixture")" == 0 ]]; then
    ok "zero clears the alarm"
else
    bad "clear left $(cat "$fixture") in the wakealarm node"
fi

# Argument validation: the wrapper is the privileged target, so
# anything that is not epoch seconds is refused before a write.
refuses "non-numeric argument is refused" "$test_wrapper" invalid

# Unwritable node: a read-only wakealarm is a failed program,
# not a silent success. Root ignores 0444, which would make this
# case report the wrapper broken when it is not — so it is
# skipped rather than inverted, and the summary says so.
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
absent_wrapper="$WORK/evo-rtc-wake.absent"
sed "s|/sys/class/rtc/rtc0/wakealarm|$WORK/no-rtc-class/rtc0/wakealarm|" \
    "$WRAPPER" > "$absent_wrapper"
chmod 0755 "$absent_wrapper"
if grep -q '/sys/class/rtc' "$absent_wrapper"; then
    bad "absent-rtc redirect failed — copy still targets the real RTC node"
else
    refuses "absent rtc0 is a failed program" "$absent_wrapper" 1700000123
fi

echo "summary: $PASS passed, $FAIL failed, $SKIP skipped"
if [[ $FAIL -eq 0 ]]; then
    echo "rtc-wake.test.sh: passed"
else
    exit 1
fi
