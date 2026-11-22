#!/usr/bin/env bash
# mpd-fragment-boots-paused.test.sh — nothing is audible before
# the startup volume has been applied.
#
# MPD starts seconds into boot and restores both its playing
# state and its own volume from its state file. The framework's
# configured startup volume cannot be applied until the
# playback.mpd plugin has loaded and read the operator's audio
# options, which is far later. A device whose last session ended
# loud is heard at that level in the gap unless MPD boots Paused.
#
# Two files decide that, and they have to agree:
#
#   * the template bootstrap installs at /etc/evo/mpd.conf, and
#   * the renderer that OVERWRITES that same path at runtime.
#
# They disagreed. The template declared `restore_paused "yes"`;
# the renderer did not, so the first fragment render dropped it
# and every boot after that resumed play. This pins both.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
TEMPLATE="$REPO_ROOT/dist/mpd/evo-fragment.conf"
RENDERER="$REPO_ROOT/plugins/org.evoframework.playback.mpd/src/mpd_fragment.rs"
BOOTSTRAP="$REPO_ROOT/dist/scripts/bootstrap.sh"

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

assert_ok "the shipped template exists" "[[ -f \"\$TEMPLATE\" ]]"
assert_ok "the template boots the device paused" \
    "grep -qE '^restore_paused \"yes\"' \"\$TEMPLATE\""

assert_ok "the renderer exists" "[[ -f \"\$RENDERER\" ]]"
# Matched loosely on purpose: the renderer holds the directive
# inside a Rust format string, so the quotes around the value
# are escaped and a fixed-string match would pin the escaping
# rather than the behaviour. The Rust pin
# `the_rendered_fragment_boots_paused` asserts the rendered
# output itself; this only catches the line going missing.
# Only the renderer proper, not its test module: the Rust pin
# asserts the same text, and a grep over the whole file would be
# satisfied by that assertion while the render site sat empty.
assert_ok "the renderer also emits it" \
    "sed '/mod tests/,\$d' \"\$RENDERER\" | grep -qE 'restore_paused .{0,3}yes'"

# The renderer replaces the template's file wholesale. Anything
# the template declares and the renderer omits survives exactly
# until the first render, which is the shape of the original
# defect — present on a fresh install, gone by the time anyone
# looks.
assert_ok "both name the same path" \
    "grep -qF '/etc/evo/mpd.conf' \"\$BOOTSTRAP\" \
       && grep -qF '/etc/evo/mpd.conf' \"\$REPO_ROOT/plugins/org.evoframework.playback.mpd/src/config.rs\""

# Guard the value, not just the key: `restore_paused "no"` is
# MPD's own default and would read as present while doing the
# opposite.
assert_ok "neither file declares the resuming value" \
    "! grep -qE '^restore_paused \"no\"' \"\$TEMPLATE\" \
       && ! sed '/mod tests/,\$d' \"\$RENDERER\" \
              | grep -qE 'restore_paused .{0,3}no'"

echo
echo "${PASS} passed, ${FAIL} failed"
[[ "${FAIL}" -eq 0 ]]
