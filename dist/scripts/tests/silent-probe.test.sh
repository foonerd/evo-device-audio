#!/usr/bin/env bash
# silent-probe.test.sh — assert dist/alsa/silent-probe.wav is
# a well-formed PCM WAV file. The bootstrap script's PCM
# playback probe and evo-install.sh's post-condition probe
# both feed this file to `aplay --dump-hw-params` so the probe
# has no host-package dependency (no Debian alsa-utils-data
# required). If the file is missing, truncated, or carries a
# malformed header, both probes silently skip — and the
# regression class they exist to catch returns. This test
# guards the artefact.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WAV_PATH="$(cd "$SCRIPT_DIR/../.." && pwd)/alsa/silent-probe.wav"

PASS=0
FAIL=0

assert() {
    local name="$1" cond="$2"
    if [[ "$cond" == "true" ]]; then
        echo "PASS  $name"
        PASS=$((PASS + 1))
    else
        echo "FAIL  $name"
        FAIL=$((FAIL + 1))
    fi
}

if [[ ! -f "$WAV_PATH" ]]; then
    echo "FAIL  silent-probe.wav missing at $WAV_PATH"
    exit 1
fi

# Size: 44-byte WAV header + small data chunk. A 16-frame
# silent payload (32 bytes data) plus 44 header bytes lands
# at 60 bytes; tolerate up to 1024 to allow future growth
# without breaking the test, but cap it so a wildly oversized
# file (e.g. an accidental commit of a real audio sample) is
# caught.
SIZE="$(stat -c %s "$WAV_PATH")"
if [[ $SIZE -ge 44 && $SIZE -le 1024 ]]; then
    assert "size sane (got: $SIZE bytes, range: [44, 1024])" "true"
else
    assert "size sane (got: $SIZE bytes, expected: [44, 1024])" "false"
fi

# Header: the first 12 bytes must be `RIFF<size32>WAVE`. Use
# `head -c` + `xxd` to compare the magic bytes.
RIFF_MAGIC="$(head -c 4 "$WAV_PATH")"
WAVE_MAGIC="$(dd if="$WAV_PATH" bs=1 skip=8 count=4 2>/dev/null)"
if [[ "$RIFF_MAGIC" == "RIFF" ]]; then
    assert "RIFF header at offset 0" "true"
else
    assert "RIFF header at offset 0 (got: $(printf '%q' "$RIFF_MAGIC"))" "false"
fi
if [[ "$WAVE_MAGIC" == "WAVE" ]]; then
    assert "WAVE marker at offset 8" "true"
else
    assert "WAVE marker at offset 8 (got: $(printf '%q' "$WAVE_MAGIC"))" "false"
fi

# Format chunk: bytes 12..15 must be `fmt ` (literal ASCII).
FMT_MAGIC="$(dd if="$WAV_PATH" bs=1 skip=12 count=4 2>/dev/null)"
if [[ "$FMT_MAGIC" == "fmt " ]]; then
    assert "fmt  chunk header at offset 12" "true"
else
    assert "fmt  chunk header at offset 12 (got: $(printf '%q' "$FMT_MAGIC"))" "false"
fi

# Functional check: aplay --dump-hw-params parses the header
# and prints the format. Pipe through grep so the test fails
# on a parse error, not just a non-zero exit.
if command -v aplay >/dev/null 2>&1; then
    set +e
    OUT="$(aplay --dump-hw-params "$WAV_PATH" 2>&1)"
    EXIT=$?
    set -e
    # Outside of a real device context, aplay still parses the
    # header and prints the format line before attempting to
    # open the (default) device. Look for the format echo.
    if printf '%s' "$OUT" | grep -q "Playing WAVE '$WAV_PATH'"; then
        assert "aplay parses the WAV header (exit=$EXIT, header line found)" "true"
    else
        assert "aplay parses the WAV header (exit=$EXIT)" "false"
    fi
else
    echo "SKIP  aplay not available — functional probe skipped"
fi

echo ""
echo "silent-probe.test.sh: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
