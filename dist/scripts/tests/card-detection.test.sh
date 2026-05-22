#!/usr/bin/env bash
# card-detection.test.sh — regression test for the audio-card
# auto-detection in dist/scripts/lib/detect-audio-card.sh.
#
# The 2026-05-21 bootstrap shipped /etc/asound.conf with
# `card "Loopback"` on every rig where snd-aloop was loaded,
# because the detection skipped `vc4hdmi` / `HDMI` but not
# `Loopback`. That regression class is the focus of this
# suite: each fixture below is the actual `aplay -l` output
# captured from one of the project's validation targets (or
# the source-role variant), and the test asserts the detector
# picks the correct non-virtual hardware card.
#
# Add a new fixture function whenever a new validation rig is
# added — the test failing on a known fixture is the catch
# that prevents this class of regression from returning.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIB_PATH="$(cd "$SCRIPT_DIR/../lib" && pwd)/detect-audio-card.sh"

# shellcheck source=../lib/detect-audio-card.sh
. "$LIB_PATH"

PASS=0
FAIL=0

assert_detection() {
    local name="$1" expected="$2" fixture="$3"
    local got
    if got="$(printf '%s' "$fixture" | detect_audio_card_from_aplay_output 2>/dev/null)"; then
        :
    else
        # Detector returned 1 (no card). Treated as the empty
        # string for comparison so the test framework can
        # assert this case via expected="".
        got=""
    fi
    if [[ "$got" == "$expected" ]]; then
        echo "PASS  $name (got: $got)"
        PASS=$((PASS + 1))
    else
        echo "FAIL  $name (expected: $expected, got: $got)"
        FAIL=$((FAIL + 1))
    fi
}

# Fixture: x86_64 desktop with HDA-Intel ALC233 + Loopback (snd-aloop loaded).
HDA_INTEL_FIXTURE="$(cat <<'EOF'
**** List of PLAYBACK Hardware Devices ****
card 0: Loopback [Loopback], device 0: Loopback PCM [Loopback PCM]
  Subdevices: 8/8
card 0: Loopback [Loopback], device 1: Loopback PCM [Loopback PCM]
  Subdevices: 8/8
card 1: PCH [HDA Intel PCH], device 0: ALC233 Analog [ALC233 Analog]
  Subdevices: 1/1
card 1: PCH [HDA Intel PCH], device 3: HDMI 0 [DELL U2722DE]
  Subdevices: 1/1
EOF
)"
assert_detection "x86_64 desktop: HDA-Intel + Loopback → PCH" "PCH" "$HDA_INTEL_FIXTURE"

# Fixture: x86_64 virtualised host with AC97 + Loopback.
VIRT_AC97_FIXTURE="$(cat <<'EOF'
**** List of PLAYBACK Hardware Devices ****
card 0: Loopback [Loopback], device 0: Loopback PCM [Loopback PCM]
  Subdevices: 8/8
card 0: Loopback [Loopback], device 1: Loopback PCM [Loopback PCM]
  Subdevices: 8/8
card 1: I82801AAICH [Intel 82801AA-ICH], device 0: Intel ICH [Intel 82801AA-ICH]
  Subdevices: 1/1
EOF
)"
assert_detection "x86_64 virtualised host: AC97 + Loopback → I82801AAICH" "I82801AAICH" "$VIRT_AC97_FIXTURE"

# Fixture: aarch64 SBC with I-Sabre Q2M DAC behind two vc4-hdmi and Loopback.
SBC_WITH_DAC_FIXTURE="$(cat <<'EOF'
**** List of PLAYBACK Hardware Devices ****
card 0: vc4hdmi0 [vc4-hdmi-0], device 0: MAI PCM i2s-hifi-0 [MAI PCM i2s-hifi-0]
  Subdevices: 1/1
card 1: vc4hdmi1 [vc4-hdmi-1], device 0: MAI PCM i2s-hifi-0 [MAI PCM i2s-hifi-0]
  Subdevices: 1/1
card 2: Loopback [Loopback], device 0: Loopback PCM [Loopback PCM]
  Subdevices: 8/8
card 2: Loopback [Loopback], device 1: Loopback PCM [Loopback PCM]
  Subdevices: 8/8
card 3: DAC [I-Sabre Q2M DAC], device 0: I-Sabre Q2M DAC i-sabre-codec-dai-0 [I-Sabre Q2M DAC i-sabre-codec-dai-0]
  Subdevices: 1/1
EOF
)"
assert_detection "aarch64 SBC: vc4-hdmi + Loopback + DAC → DAC" "DAC" "$SBC_WITH_DAC_FIXTURE"

# Fixture: aarch64 SBC with reference DAC and snd-aloop loaded
# for source-role fan-out. Same hardware enumeration as the
# previous fixture but exercised separately to confirm the
# source-role multiroom path doesn't accidentally pick Loopback.
SBC_SOURCE_ROLE_FIXTURE="$SBC_WITH_DAC_FIXTURE"
assert_detection "aarch64 SBC (source-role with snd-aloop): → DAC (not Loopback)" "DAC" "$SBC_SOURCE_ROLE_FIXTURE"

# Fixture: HDMI-only headless box. Fallback to first card with
# a WARN; the test asserts the fallback name is emitted.
HDMI_ONLY_FIXTURE="$(cat <<'EOF'
**** List of PLAYBACK Hardware Devices ****
card 0: vc4hdmi0 [vc4-hdmi-0], device 0: MAI PCM i2s-hifi-0 [MAI PCM i2s-hifi-0]
  Subdevices: 1/1
card 1: vc4hdmi1 [vc4-hdmi-1], device 0: MAI PCM i2s-hifi-0 [MAI PCM i2s-hifi-0]
  Subdevices: 1/1
EOF
)"
assert_detection "HDMI-only headless box: fallback → vc4hdmi0" "vc4hdmi0" "$HDMI_ONLY_FIXTURE"

# Fixture: Loopback-only host (snd-aloop loaded, no hardware
# audio). Same fallback semantics; the operator gets a WARN
# but the install proceeds (override with --card if wrong).
LOOPBACK_ONLY_FIXTURE="$(cat <<'EOF'
**** List of PLAYBACK Hardware Devices ****
card 0: Loopback [Loopback], device 0: Loopback PCM [Loopback PCM]
  Subdevices: 8/8
EOF
)"
assert_detection "Loopback-only host: fallback → Loopback" "Loopback" "$LOOPBACK_ONLY_FIXTURE"

# Fixture: empty aplay output (no cards). Detector returns 1;
# the test asserts the empty-string-as-no-card contract.
NO_CARDS_FIXTURE=""
assert_detection "No cards: detector returns 1 (empty output)" "" "$NO_CARDS_FIXTURE"

# Fixture: only the aplay header, no card lines. Same contract.
HEADER_ONLY_FIXTURE="$(cat <<'EOF'
**** List of PLAYBACK Hardware Devices ****
EOF
)"
assert_detection "Header-only output: detector returns 1" "" "$HEADER_ONLY_FIXTURE"

echo ""
echo "card-detection.test.sh: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
