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

# ---- named install defects still fail the install ----
assert_count "named defect: Database corrupted" \
    1 'mpd[1]: Database corrupted'
assert_count "named defect: Bind failed" \
    1 'mpd[1]: Bind failed: Address already in use'
assert_count "named defect: Config error" \
    1 'mpd[1]: Config error: line 3: unknown setting'
assert_count "named defect: missing music directory" \
    1 'mpd[1]: exception: Failed to open "/var/lib/evo/music": No such file or directory'

# ---- mixed: the audio-open line must not mask a real defect ----
assert_count "audio-open alongside a real defect: only the defect counts" \
    1 "$(printf '%s\n%s\n' "$VM_LINE" 'mpd[1]: Database corrupted')"
assert_count "every named defect at once" \
    4 "$(printf '%s\n%s\n%s\n%s\n' \
        'mpd[1]: Database corrupted' \
        'mpd[1]: Bind failed: Address already in use' \
        'mpd[1]: Config error: line 3' \
        'mpd[1]: exception: Failed to open "/var/lib/evo/music": No such file or directory')"

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
