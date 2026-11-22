#!/usr/bin/env bash
# steward-units-set-no-tz.test.sh — no unit source this
# distribution ships sets TZ for the steward.
#
# The steward reads local time from /etc/localtime, and a zone
# change reaches it within a second because chrono re-checks that
# file. A TZ in the steward's environment takes precedence over
# /etc/localtime, so timezone.set would apply a zone the steward
# never follows and the alarm would be rescheduled on the old one.
#
# Every unit source under dist/systemd/ is read. A line that sets
# TZ through Environment=, PassEnvironment= or DefaultEnvironment=
# is a failure. So is EnvironmentFile=: the file it names is not a
# unit source, so the claim could no longer be shown here.
#
# Invoke from anywhere:
#
#   bash dist/scripts/tests/steward-units-set-no-tz.test.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
UNITS_DIR="$(cd "$SCRIPT_DIR/../../systemd" && pwd)"

PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "PASS  $1"; }
bad() { FAIL=$((FAIL + 1)); echo "FAIL  $1" >&2; }

# Print every offending line under a directory, as file:line:text.
offences() {
    grep -rnE \
        -e '^[[:space:]]*(Default)?Environment=(.*[[:space:]])?"?TZ=' \
        -e '^[[:space:]]*PassEnvironment=(.*[[:space:]])?TZ([[:space:]]|$)' \
        -e '^[[:space:]]*EnvironmentFile=' \
        "$1" || true
}

units="$(find "$UNITS_DIR" -type f | sort)"
if [[ -z "$units" ]] || ! grep -q '/evo.service$' <<<"$units"; then
    bad "no steward unit found under $UNITS_DIR"
else
    ok "reads $(wc -l <<<"$units") unit sources under dist/systemd"
fi

found="$(offences "$UNITS_DIR")"
if [[ -z "$found" ]]; then
    ok "no unit source sets TZ or names an EnvironmentFile"
else
    bad "a unit source can set TZ for the steward:"
    printf '%s\n' "$found" >&2
fi

# Control: the scan must be shown able to find each form before
# its silence above counts for anything.
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
for line in \
    'Environment=TZ=UTC' \
    'Environment="LANG=C" "TZ=Europe/London"' \
    'PassEnvironment=LANG TZ' \
    'PassEnvironment=TZ' \
    'DefaultEnvironment=TZ=UTC' \
    'EnvironmentFile=-/etc/default/evo'; do
    printf '[Service]\n%s\n' "$line" > "$WORK/probe.conf"
    if [[ -n "$(offences "$WORK")" ]]; then
        ok "the scan finds: $line"
    else
        bad "the scan misses: $line"
    fi
done
printf '[Service]\nEnvironment="EVO_TZ_HINT=1"\nUnsetEnvironment=TZ\n' > "$WORK/probe.conf"
if [[ -z "$(offences "$WORK")" ]]; then
    ok "the scan passes a unit that only unsets TZ"
else
    bad "the scan flags a unit that only unsets TZ"
fi

echo "summary: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
