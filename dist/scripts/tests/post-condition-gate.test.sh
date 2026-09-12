#!/usr/bin/env bash
# post-condition-gate.test.sh — regression test for which
# post-condition findings fail the install primitive.
#
# The gate block is extracted from the shipped evo-install.sh at
# test time and evaluated against synthetic finding sets, so the
# test cannot drift from the file it guards: re-adding a gate
# here makes a case below fail.
#
# The class this exists for: a host whose only enumerated output
# is HDMI can have the steward active and every declared plugin
# admitted, and still not open pcm.evo. That used to exit 5 and
# send the operator back to curl for a machine that was actually
# installed and running. The listening device is chosen in
# Settings → System → Audio; a probe result is evidence, not a
# verdict on the install.
#
# What must still fail is the other half of the contract, and
# each of those has a case here too.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALLER="$(cd "$SCRIPT_DIR/.." && pwd)/evo-install.sh"

# Extract `POST_OK=1` through the line before `emit_evidence`.
GATE_BLOCK="$(awk '/^POST_OK=1$/ { on = 1 } on && /^emit_evidence$/ { exit } on' \
    "$INSTALLER")"
if [[ -z "$GATE_BLOCK" ]]; then
    echo "FAIL  could not extract the POST_OK block from $INSTALLER" >&2
    exit 1
fi

PASS=0
FAIL=0

# A host that installed correctly. Every case below starts here
# and changes one finding.
#
# These are read by the gate block this test evaluates, which
# is why static analysis cannot see the use from here. They are
# the post-condition findings evo-install.sh sets before the
# block runs; a name that drifts out of the installer shows up
# as a case flipping, not as a silent no-op, because every case
# below asserts a value only that finding can produce.
#
# shellcheck disable=SC2034
healthy_env() {
    ACTIVE_STATE="active"
    PLUGINS_ADMITTED=19
    PLUGINS_EXPECTED=19
    ADMISSION_FAILURES=0
    NOT_DECLARED=0
    JOURNAL_FAIL_COUNT=0
    PCM_PLAYBACK_PROBE="ok"
    SMB_NETBIOS_CHECK="ok"
    LAN_DISCOVERY_CHECK="ok"
    STORAGE_USB_PROVISIONING_CHECK="ok"
    MUSIC_HASH_PRESERVED="true"
    MODE="p1"
    EVO_UI_CHECK="ok"
    EVO_KIOSK_CHECK="ok"
    SHELL_FETCH="ok"
}

assert_gate() {
    local name="$1" expect="$2"; shift 2
    local got
    # Evaluate the extracted block in a subshell and report the
    # value, not an exit status — POST_OK=1 means success, which
    # as an exit code would mean the opposite.
    got="$(
        set +eu
        healthy_env
        # Per-case overrides, e.g. PCM_PLAYBACK_PROBE=fail
        for kv in "$@"; do eval "$kv"; done
        eval "$GATE_BLOCK"
        printf '%s' "$POST_OK"
    )"
    if [[ "$got" == "$expect" ]]; then
        echo "PASS  $name (POST_OK=$got)"
        PASS=$((PASS + 1))
    else
        echo "FAIL  $name (expected POST_OK=$expect, got $got)"
        FAIL=$((FAIL + 1))
    fi
}

# Baseline.
assert_gate "healthy host → install succeeds" 1

# The cut: a probe failure is evidence, not a verdict.
assert_gate "HDMI-only host: steward active, full admission, pcm fail → succeeds" \
    1 'PCM_PLAYBACK_PROBE="fail"'
assert_gate "pcm busy (MPD holds the device) → succeeds" \
    1 'PCM_PLAYBACK_PROBE="busy"'
assert_gate "pcm skipped (no aplay) → succeeds" \
    1 'PCM_PLAYBACK_PROBE="skipped_no_aplay"'

# The other half of the contract, unchanged.
assert_gate "dead steward → fails" 0 'ACTIVE_STATE="inactive"'
assert_gate "no plugins admitted → fails" 0 'PLUGINS_ADMITTED=0'
assert_gate "short admission → fails" 0 'PLUGINS_ADMITTED=18'
assert_gate "admission failures → fails" 0 'ADMISSION_FAILURES=1'
assert_gate "undeclared plugin → fails" 0 'NOT_DECLARED=1'
assert_gate "journal failures → fails" 0 'JOURNAL_FAIL_COUNT=3'
assert_gate "netbios mismatch → fails" 0 'SMB_NETBIOS_CHECK="mismatch"'
assert_gate "LAN discovery degraded → fails" 0 'LAN_DISCOVERY_CHECK="degraded"'
assert_gate "USB provisioning degraded → fails" 0 'STORAGE_USB_PROVISIONING_CHECK="degraded"'

# A dead steward on an HDMI-only box still fails: demoting the
# probe must not mask a real failure that happens alongside it.
assert_gate "pcm fail AND dead steward → still fails" \
    0 'PCM_PLAYBACK_PROBE="fail"' 'ACTIVE_STATE="inactive"'
assert_gate "pcm fail AND short admission → still fails" \
    0 'PCM_PLAYBACK_PROBE="fail"' 'PLUGINS_ADMITTED=18'

# Wipe modes keep the music-library invariant.
assert_gate "wipe-config with music hash changed → fails" \
    0 'MODE="wipe-config"' 'MUSIC_HASH_PRESERVED="false"'
assert_gate "p1 ignores the music-hash invariant" \
    1 'MODE="p1"' 'MUSIC_HASH_PRESERVED="false"'

assert_gate "dead evo-ui → fails" 0 'EVO_UI_CHECK="inactive"'
assert_gate "shell refused → fails" 0 'SHELL_FETCH="refused"'
assert_gate "dead evo-kiosk → fails" 0 'EVO_KIOSK_CHECK="inactive"'
assert_gate "headless compose (no ui/kiosk units) → succeeds" \
    1 'EVO_UI_CHECK="absent"' 'EVO_KIOSK_CHECK="absent"' 'SHELL_FETCH="skipped"'

if grep -q 'start_operator_surface' "$INSTALLER" \
    && grep -q 'systemctl restart evo-ui.service' "$INSTALLER" \
    && grep -q 'systemctl restart evo-kiosk.service' "$INSTALLER"; then
    echo "PASS  installer restarts evo-ui and evo-kiosk after stop (POST_OK=n/a)"
    PASS=$((PASS + 1))
else
    echo "FAIL  installer does not restart evo-ui and evo-kiosk"
    FAIL=$((FAIL + 1))
fi

echo ""
echo "post-condition-gate.test.sh: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
