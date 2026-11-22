#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Just a Nerd
#
# set-cut-version.sh — cut(V): write the named cut through this repo.
#
# There is one operation, cut(V). V is the only input
# (MAJOR.MINOR.PATCH.CUT). Slot keys and piece-pins for the
# pieces this cut mints take that V. Cargo stays three components.
#
# Writes:
#   CUT_VERSION                         V
#   scripts/release/piece-pins.toml     steward/ui/kiosk/dist → V
#                                       boot pin left alone
#
# Usage: scripts/release/set-cut-version.sh V
set -euo pipefail

V="${1:-}"
if ! [[ "${V}" =~ ^[0-9]+\.[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "cut(V): V must be MAJOR.MINOR.PATCH.CUT, got '${V}'" >&2
    exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

printf '%s\n' "${V}" > CUT_VERSION

PINS=scripts/release/piece-pins.toml
[[ -f "${PINS}" ]] || { echo "cut(V): missing ${PINS}" >&2; exit 2; }

python3 - "${PINS}" "${V}" <<'PY'
import re, sys
path, v = sys.argv[1], sys.argv[2]
src = open(path).read()
# Pins that move with every cut(V). Boot stays on its last slot
# unless a later sitting changes the boot piece.
for key in (
    "evo-device-audio",
    "evo-ui-shell",
    "evo-ui-runtime",
    "evo-kiosk",
    "evo-device-audio-dist",
):
    pat = rf'(?m)^({re.escape(key)}\s*=\s*")[^"]*(")'
    out, n = re.subn(pat, rf'\g<1>{v}\2', src, count=1)
    if n != 1:
        sys.exit(f"cut(V): no pin line for {key} in {path}")
    src = out
open(path, "w").write(src)
PY

echo "cut(${V}): CUT_VERSION=${V}; piece-pins steward/ui/kiosk/dist → ${V}; boot pin unchanged"
