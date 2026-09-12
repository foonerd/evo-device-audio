#!/usr/bin/env bash
# stage-evo-service.test.sh — first-boot tree must carry the
# main unit. evo-install.sh install_main_systemd_unit() stats
# dist/systemd/evo.service; a tree with only evo.service.d/
# dies at [7/8] after wipe_config has already removed drop-ins.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
STAGE="$REPO_ROOT/scripts/release/stage-audio-dist-tree.sh"

PASS=0
FAIL=0
pass() { echo "PASS  $*"; PASS=$((PASS + 1)); }
fail() { echo "FAIL  $*"; FAIL=$((FAIL + 1)); }

if [[ -f "$REPO_ROOT/dist/systemd/evo.service" ]]; then
    pass "repo carries dist/systemd/evo.service"
else
    fail "repo missing dist/systemd/evo.service"
fi

OUT="$(mktemp -d)"
cleanup() { rm -rf "$OUT"; }
trap cleanup EXIT

bash "$STAGE" --repo-root "$REPO_ROOT" --out-dir "$OUT"
if [[ -f "$OUT/dist/systemd/evo.service" ]]; then
    pass "stage writes dist/systemd/evo.service"
else
    fail "stage wrote no dist/systemd/evo.service"
fi
if [[ -f "$OUT/dist/systemd/evo.service.d/exec-start.conf" ]]; then
    pass "stage still writes exec-start.conf"
else
    fail "stage dropped exec-start.conf"
fi

echo "summary: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
