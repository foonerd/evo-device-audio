# shellcheck shell=bash
#
# Pure function: parse `aplay -l` output (passed via stdin)
# and emit the chosen ALSA card NAME on stdout. Returns 0 on a
# clean pick, 0 on a fallback pick (with a WARN emitted to
# stderr), and 1 when no card is available. The function does
# NOT touch real state — it is the testable kernel of the
# bootstrap script's card detection. Sourced by bootstrap.sh
# and by the regression tests under dist/scripts/tests/.
#
# Filter classes (documented at the top so the rationale
# travels with the implementation):
#
#   1. `Loopback` — created by snd-aloop. The multiroom
#      source-role path loads snd-aloop deliberately as a
#      fan-out target; on every other rig with the module
#      loaded (often as card 0 because module-load.d orders
#      it ahead of platform cards), an unconditional first-
#      pick would write the loopback name into pcm.evo and
#      silently route audio to a virtual sink instead of the
#      DAC.
#   2. `vc4hdmi*` / any name containing `HDMI` — Pi-class and
#      Intel-iGPU boards enumerate HDMI before the attached
#      audio DAC; the operator's intent for a music appliance
#      is the DAC, not the display's speakers. Operators with
#      HDMI-as-intended-output (e.g. AVR via HDMI) override
#      with --card.
#
# If only filtered cards are present (HDMI-only headless,
# Loopback-only sound stack), the function falls back to
# first-card to keep the install primitive working — but the
# operator gets a visible WARN that lists what was filtered,
# so the silent-wrong-card class of regression cannot recur.

detect_audio_card_from_aplay_output() {
    local aplay_output
    aplay_output="$(cat)"
    local card
    # Case-insensitive HDMI match via tolower() — the awk
    # `/HDMI/i` regex-flag form is gawk-only and silently
    # misparses on mawk (Debian default), where the trailing
    # `i` triggers an unrelated coercion that yields false
    # positives on unrelated strings (e.g. "I82801AAICH"
    # registered as HDMI). tolower() + lowercase regex is
    # portable across both implementations.
    card="$(printf '%s\n' "$aplay_output" | awk -F'[: ]+' '
        /^card [0-9]+/ {
            name = $3
            lname = tolower(name)
            if (lname !~ /^vc4hdmi/ \
                && lname !~ /hdmi/ \
                && name != "Loopback") {
                print name
                exit
            }
        }
    ')"
    if [[ -n "$card" ]]; then
        printf '%s\n' "$card"
        return 0
    fi
    card="$(printf '%s\n' "$aplay_output" \
        | awk -F'[: ]+' '/^card [0-9]+/ { print $3; exit }')"
    if [[ -n "$card" ]]; then
        echo "[bootstrap] WARN: only filtered cards detected (HDMI / Loopback);" >&2
        echo "                  fell back to first card '$card'. Override" >&2
        echo "                  with --card <NAME> if this is wrong." >&2
        printf '%s\n' "$card"
        return 0
    fi
    return 1
}
