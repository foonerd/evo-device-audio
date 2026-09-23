#!/usr/bin/env bash
# overlay-dist-install-surface.test.sh — compose must overlay
# this repo's install primitives onto a frozen dist piece.
# A piece-only tree still carries the unbounded chown and
# lacks the RTC wrapper; shipping that bootstrap bricks
# wipe-config on a box with an adopt.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
OVERLAY="${REPO_ROOT}/scripts/release/overlay-dist-install-surface.sh"
COMPOSE="${REPO_ROOT}/scripts/release/compose-distribution-from-pieces.sh"

PASS=0
FAIL=0
pass() { echo "PASS  $*"; PASS=$((PASS + 1)); }
fail() { echo "FAIL  $*"; FAIL=$((FAIL + 1)); }

if grep -q 'overlay-dist-install-surface.sh' "$COMPOSE"; then
    pass "compose calls the install-surface overlay"
else
    fail "compose does not call overlay-dist-install-surface.sh"
fi

# A frozen-piece-shaped tree: old bootstrap, no chown lib, no RTC.
DEST="$(mktemp -d)"
cleanup() { rm -rf "$DEST"; }
trap cleanup EXIT

mkdir -p "$DEST/scripts/lib" "$DEST/bin" "$DEST/sudoers.d" "$DEST/systemd"
cat > "$DEST/scripts/bootstrap.sh" <<'OLD'
#!/usr/bin/env bash
chown -R "$SERVICE_USER:$SERVICE_USER" /var/lib/evo
OLD
printf 'old-captive\n' > "$DEST/bin/evo-captive-probe"
printf 'old-kiosk\n' > "$DEST/sudoers.d/evo-system-kiosk.in"
mkdir -p "$DEST/systemd/evo.service.d"
printf 'exec\n' > "$DEST/systemd/evo.service.d/exec-start.conf"

bash "$OVERLAY" --repo-root "$REPO_ROOT" --dest-dist "$DEST"

if grep -q 'chown_tree_same_fs /var/lib/evo' "$DEST/scripts/bootstrap.sh" \
    && ! grep -qE 'chown[[:space:]]+-R[[:space:]]+"\$SERVICE_USER:\$SERVICE_USER"[[:space:]]+/var/lib/evo' \
        "$DEST/scripts/bootstrap.sh"; then
    pass "overlay replaces piece bootstrap (no unbounded chown)"
else
    fail "overlay left the piece bootstrap in place"
fi

if [[ -f "$DEST/scripts/lib/chown-tree-same-fs.sh" ]]; then
    pass "overlay places chown-tree-same-fs.sh"
else
    fail "overlay missing chown-tree-same-fs.sh"
fi

if [[ -f "$DEST/scripts/lib/chown-tenant-state-trees.sh" ]]; then
    pass "overlay places chown-tenant-state-trees.sh"
else
    fail "overlay missing chown-tenant-state-trees.sh"
fi

if [[ -x "$DEST/bin/evo-rtc-wake" ]]; then
    pass "overlay places evo-rtc-wake"
else
    fail "overlay missing evo-rtc-wake"
fi

if [[ -f "$DEST/sudoers.d/evo-rtc-wake.in" ]]; then
    pass "overlay places evo-rtc-wake.in"
else
    fail "overlay missing evo-rtc-wake.in"
fi

if [[ -f "$DEST/systemd/evo.service" ]]; then
    pass "overlay places evo.service"
else
    fail "overlay missing evo.service"
fi

if [[ -f "$DEST/bin/evo-captive-probe" ]] \
    && grep -qx 'old-captive' "$DEST/bin/evo-captive-probe"; then
    pass "overlay does not replace piece bin/evo-captive-probe"
else
    fail "overlay disturbed evo-captive-probe"
fi

if grep -qx 'old-kiosk' "$DEST/sudoers.d/evo-system-kiosk.in"; then
    pass "overlay does not replace piece kiosk sudoers"
else
    fail "overlay overwrote evo-system-kiosk.in"
fi

if [[ -f "$DEST/systemd/evo.service.d/exec-start.conf" ]]; then
    pass "overlay leaves systemd drop-ins from the piece"
else
    fail "overlay dropped exec-start.conf"
fi

echo "summary: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
