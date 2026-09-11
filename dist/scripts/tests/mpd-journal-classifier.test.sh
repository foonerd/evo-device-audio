#!/usr/bin/env bash
# mpd-journal-classifier.test.sh — which mpd journal lines are
# install defects, and which are facts with another owner.
#
# The class this exists for: "Failed to open audio output" had
# TWO evaluators reaching opposite verdicts on one fact. The PCM
# probe owns it (verify_pcm_playback) and correctly treats it as
# evidence, not a verdict — post-condition-gate.test.sh pins
# that. The journal scan ALSO read it, as any-line-matching-fail
# minus a two-entry whitelist, and failed POST_OK. A live VM
# wipe-config on 2026-09-11 came out pcm.evo=ok, 19/19 admitted,
# music hash preserved, every service active — and exited 5, on
# that one mpd line. A false FAIL sends an operator to
# --mode=reinstall on a machine that installed correctly.
#
# The classifier is extracted from the shipped evo-install.sh at
# test time, so this cannot drift from the file it guards.
#
# THREE SHIP SURFACES emit that line, and none of them is a
# failed install when the steward, admission and the operator
# surface are up:
#   - new Trixie minimal, nothing configured
#   - existing Trixie already running Pulse/PipeWire/unknown
#   - prior evo box with a valid or broken asound
# Audio output device selection is Settings -> System -> Audio.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALLER="$(cd "$SCRIPT_DIR/.." && pwd)/evo-install.sh"

# Pull the function out of the shipped installer, not a copy of
# its predicate.
CLASSIFIER="$(awk '/^mpd_journal_defects\(\) \{$/ { on = 1 } on { print } on && /^\}$/ { exit }' \
    "$INSTALLER")"
if [[ -z "$CLASSIFIER" ]]; then
    echo "FAIL  could not extract mpd_journal_defects from $INSTALLER" >&2
    exit 1
fi
eval "$CLASSIFIER"

PASS=0
FAIL=0

# Count how many lines the classifier calls defects. This is the
# number that becomes JOURNAL_FAIL_COUNT, which the POST_OK gate
# fails on when > 0.
defect_count() {
    local hits
    hits="$(printf '%s\n' "$1" | mpd_journal_defects)"
    if [[ -z "$hits" ]]; then printf '0'; else printf '%s' "$(printf '%s\n' "$hits" | grep -c .)"; fi
}

assert_count() {
    local name="$1" expect="$2" input="$3"
    local got; got="$(defect_count "$input")"
    if [[ "$got" == "$expect" ]]; then
        echo "PASS  $name (defects=$got)"
        PASS=$((PASS + 1))
    else
        echo "FAIL  $name (expected $expect, got $got)"
        FAIL=$((FAIL + 1))
    fi
}

# ---- the failing case: the exact line off the 2026-09-11 VM ----
VM_LINE='Sep 11 10:47:43 player mpd[7548]: exception: Failed to open audio output'

assert_count "ship surface: new Trixie minimal — audio output unopened is not an install defect" \
    0 "$VM_LINE"
assert_count "ship surface: existing Trixie (Pulse/PipeWire/unknown) — same line, same verdict" \
    0 'Sep 11 10:47:43 host mpd[1]: exception: Failed to open audio output'
assert_count "ship surface: prior evo box with a broken asound — same line, same verdict" \
    0 'Sep 11 10:47:43 player mpd[999]: exception: Failed to open audio output'

# ---- baseline mpd first-boot noise stays ignored ----
assert_count "fresh /var/lib/mpd: tag_cache absent is ignored" \
    0 'mpd[1]: exception: Failed to open "/var/lib/mpd/tag_cache": No such file or directory'
assert_count "fresh /var/lib/mpd: state absent is ignored" \
    0 'mpd[1]: exception: Failed to open "/var/lib/mpd/state": No such file or directory'

# ---- fatal defects, in MPD 0.24.4's OWN words ----
# Every token below was read out of `strings /usr/bin/mpd` on the
# VM. `Bind failed` and `Config error` are NOT in that binary —
# they came from a comment, and fixtures asserting them taught the
# lie. A predicate built on them let a real bind failure PASS.
assert_count "fatal token: Database corrupted" \
    1 'mpd[1]: Database corrupted'
assert_count "fatal token: Failed to bind socket" \
    1 'mpd[1]: exception: Failed to bind socket'
assert_count "fatal token: Failed to bind to '<addr>'" \
    1 "mpd[1]: exception: Failed to bind to '0.0.0.0:6600'"
assert_count "fatal token: unrecognized parameter" \
    1 'mpd[1]: unrecognized parameter: "not_a_setting"'
assert_count "fatal token: Error in <file> line <n>" \
    1 'mpd[1]: Error in "/etc/mpd.conf" line 3'
assert_count "fatal token: configuration file does not exist" \
    1 'mpd[1]: configuration file does not exist: /etc/mpd.conf'
assert_count "fatal token: missing music directory (path-keyed)" \
    1 'mpd[1]: exception: Failed to open "/var/lib/evo/music": No such file or directory'

# ---- non-fatal neighbours in the SAME binary: must not count ----
# MPD says these and keeps running. Counting them would be the
# false-FAIL defect again, wearing bind's clothes.
assert_count "non-fatal neighbour: bind to one address failed, another succeeded" \
    0 "mpd[1]: bind to '1.2.3.4' failed (continuing anyway, because binding to '0.0.0.0' succeeded): Cannot assign requested address"
assert_count "non-fatal neighbour: Failed to listen (not fatal)" \
    0 'mpd[1]: Failed to listen on /run/mpd/socket (not fatal)'

# ---- mixed: the audio-open line must not mask a real defect ----
assert_count "audio-open alongside a real defect: only the defect counts" \
    1 "$(printf '%s\n%s\n' "$VM_LINE" 'mpd[1]: Database corrupted')"
assert_count "every fatal token at once" \
    7 "$(printf '%s\n%s\n%s\n%s\n%s\n%s\n%s\n' \
        'mpd[1]: Database corrupted' \
        'mpd[1]: exception: Failed to bind socket' \
        "mpd[1]: exception: Failed to bind to '0.0.0.0:6600'" \
        'mpd[1]: unrecognized parameter: "not_a_setting"' \
        'mpd[1]: Error in "/etc/mpd.conf" line 3' \
        'mpd[1]: configuration file does not exist: /etc/mpd.conf' \
        'mpd[1]: exception: Failed to open "/var/lib/evo/music": No such file or directory')"

# The regression this row exists for: the retired fail(ed|ure)?
# scan WOULD have counted a real bind failure. eff8ade's predicate
# did not. A false PASS is the worse half of the same bug.
assert_count "bind failure alongside the benign audio-open line: bind still counts" \
    1 "$(printf '%s\n%s\n' "$VM_LINE" 'mpd[1]: exception: Failed to bind socket')"

# ---- the whole VM journal, as it actually read ----
assert_count "the 2026-09-11 VM journal in full → install is not failed" \
    0 "$(printf '%s\n%s\n%s\n' \
        'mpd[1]: exception: Failed to open "/var/lib/mpd/tag_cache": No such file or directory' \
        'mpd[1]: exception: Failed to open "/var/lib/mpd/state": No such file or directory' \
        "$VM_LINE")"

# ---- the evo arm is a separate owner and is NOT this function ----
if grep -qE "fail_evo=\\\$\\(journalctl -u evo .* \\| grep -iE 'fail\\(ed\\|ure\\)\\?" "$INSTALLER"; then
    echo "PASS  evo journal arm untouched: a real evo fail line still counts"
    PASS=$((PASS + 1))
else
    echo "FAIL  the evo journal arm changed shape"
    FAIL=$((FAIL + 1))
fi

# ---- the fictional tokens must never return ----
# Scope this to the predicate BODY. The function's comment names
# both fictional tokens on purpose, to record why they are not
# there; asserting against the whole file would match that prose.
CLASSIFIER_BODY="$(printf '%s\n' "$CLASSIFIER" | sed '/^#/d')"
if printf '%s\n' "$CLASSIFIER_BODY" | grep -qE "Bind failed|Config error"; then
    echo "FAIL  a token that is not in the MPD binary is back in the predicate"
    FAIL=$((FAIL + 1))
else
    echo "PASS  Bind failed / Config error absent from the predicate: it speaks MPD's words"
    PASS=$((PASS + 1))
fi
if printf '%s\n' "$CLASSIFIER_BODY" | grep -qE "fail\(ed\|ure\)\?"; then
    echo "FAIL  the fail(ed|ure)? substring scan is back in the predicate"
    FAIL=$((FAIL + 1))
else
    echo "PASS  fail(ed|ure)? substring scan absent from the predicate"
    PASS=$((PASS + 1))
fi

# ---- the old path must be gone, not merely bypassed ----
if grep -qE "grep -vE 'exception: Failed to open \"/var/lib/mpd/" "$INSTALLER"; then
    echo "FAIL  the substring+whitelist scan is still in the installer"
    FAIL=$((FAIL + 1))
else
    echo "PASS  substring+whitelist scan retired (no second classifier)"
    PASS=$((PASS + 1))
fi

echo ""
echo "mpd-journal-classifier.test.sh: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
