#!/usr/bin/env bash
# install-updater.test.sh — the updater helper's placement.
#
# Runs install-updater.sh against a prefix tree (EVO_INSTALL_ROOT), so no
# root and no systemd are needed, and pins what the installer must hold:
# the helper and worker land, the allow list is replaced but a lab key in
# trust.d and an operator's updater.toml are kept, a bad service user or a
# missing binary is refused, and allow.toml names only files `dist/`
# actually ships (a name with no source would make every carrier fail).
#
#   bash dist/scripts/tests/install-updater.test.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALLER="$DIST_DIR/scripts/install-updater.sh"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

PASS=0; FAIL=0
ok()   { PASS=$((PASS + 1)); echo "PASS  $1"; }
fail() { FAIL=$((FAIL + 1)); echo "FAIL  $1"; }
check() { if "${@:2}"; then ok "$1"; else fail "$1"; fi; }

BIN="$WORK/bin"; mkdir -p "$BIN"
printf '#!/bin/sh\n' > "$BIN/evo-apply-update"
printf '#!/bin/sh\n' > "$BIN/evo-apply-update-worker"
chmod 0755 "$BIN"/*
R="$WORK/root"

run() { EVO_INSTALL_ROOT="$R" bash "$INSTALLER" --dist "$DIST_DIR" --bin-dir "$BIN" --service-user "$1" "${@:2}"; }

run evotest >/dev/null
check "the sudo target is placed 0755" test "$(stat -c %a "$R/usr/local/bin/evo-apply-update")" = 755
check "the worker is placed outside sudoers' reach, under libexec" test -x "$R/usr/libexec/evo-apply-update/evo-apply-update-worker"
check "the unit is placed" test -f "$R/etc/systemd/system/evo-apply-update@.service"
check "the sudoers grant is 0440 and names the service user and only the wrapper" \
    bash -c "test \"\$(stat -c %a '$R/etc/sudoers.d/evo-apply-update')\" = 440 && grep -qx 'evotest ALL=(root) NOPASSWD: /usr/local/bin/evo-apply-update' '$R/etc/sudoers.d/evo-apply-update' && ! grep -q NOPASSWD:.*worker '$R/etc/sudoers.d/evo-apply-update'"
check "the worker is not in any sudoers grant" bash -c "! grep -rq 'evo-apply-update-worker' '$R/etc/sudoers.d'"
check "the commons key and its sidecar are in the updater's own trust.d" \
    test -f "$R/etc/evo-updater/trust.d/commons-plugin-signing-public.pem" -a -f "$R/etc/evo-updater/trust.d/commons-plugin-signing-public.meta.toml"
check "the state directory exists" test -d "$R/var/lib/evo-updater"

# A rerun replaces the allow list, keeps an edited updater.toml and a lab key.
echo 'tampered' > "$R/etc/evo-updater/allow.toml"
echo 'base_url = "https://lab.example"' > "$R/etc/evo-updater/updater.toml"
echo 'lab key' > "$R/etc/evo-updater/trust.d/lab.pem"
run evotest >/dev/null
check "a rerun restores the allow list from the installer" cmp -s "$DIST_DIR/etc-updater/allow.toml" "$R/etc/evo-updater/allow.toml"
check "a rerun keeps the operator's updater.toml" grep -q 'lab.example' "$R/etc/evo-updater/updater.toml"
check "a rerun keeps a lab key in trust.d" test -f "$R/etc/evo-updater/trust.d/lab.pem"

# Refusals.
if run 'bad user' >/dev/null 2>&1; then fail "a service user with a space is refused"; else ok "a service user with a space is refused"; fi
if run 'Root;id' >/dev/null 2>&1; then fail "a service user with shell characters is refused"; else ok "a service user with shell characters is refused"; fi
rm "$BIN/evo-apply-update-worker"
if run evotest >/dev/null 2>&1; then fail "a missing worker binary is refused"; else ok "a missing worker binary is refused"; fi

# allow.toml must name only files dist/ ships.
missing=0
while IFS=$'\t' read -r kind name; do
    case "$kind" in
        bin) [[ -f "$DIST_DIR/bin/$name" ]] || { echo "  no dist/bin/$name"; missing=1; } ;;
        sudoers) [[ -f "$DIST_DIR/sudoers.d/$name.in" ]] || { echo "  no dist/sudoers.d/$name.in"; missing=1; } ;;
        *) echo "  unknown kind $kind"; missing=1 ;;
    esac
done < <(python3 - "$DIST_DIR/etc-updater/allow.toml" <<'PY'
import sys, tomllib
for f in tomllib.load(open(sys.argv[1], 'rb'))['file']:
    print(f"{f['kind']}\t{f['name']}")
PY
)
check "every allowed file has a source in dist/" test "$missing" = 0

# The unit uses the literal instance (%i), never the path-unescaped %I.
check "the unit passes the instance with %i, not %I" bash -c "grep -q 'worker %i' '$DIST_DIR/systemd/evo-apply-update@.service' && ! grep -q '%I' '$DIST_DIR/systemd/evo-apply-update@.service'"
check "the unit runs as root as a oneshot" bash -c "grep -qx 'User=root' '$DIST_DIR/systemd/evo-apply-update@.service' && grep -qx 'Type=oneshot' '$DIST_DIR/systemd/evo-apply-update@.service'"

echo ""
echo "install-updater: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
