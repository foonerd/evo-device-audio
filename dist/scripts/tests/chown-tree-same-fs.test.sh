#!/usr/bin/env bash
# chown-tree-same-fs.test.sh — bootstrap must not recursively
# chown a USB or NAS adopt. The walk stays on the filesystem
# that holds the state tree; a read-only foreign mount must
# not abort the install.
#
# Extracts nothing from bootstrap.sh except the structural
# lock; the function under test is the sourced lib.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIB_PATH="$(cd "$SCRIPT_DIR/../lib" && pwd)/chown-tree-same-fs.sh"
BOOTSTRAP="$(cd "$SCRIPT_DIR/.." && pwd)/bootstrap.sh"

# shellcheck source=../lib/chown-tree-same-fs.sh
. "$LIB_PATH"

PASS=0
FAIL=0

pass() { echo "PASS  $*"; PASS=$((PASS + 1)); }
fail() { echo "FAIL  $*"; FAIL=$((FAIL + 1)); }

if grep -qE 'chown[[:space:]]+-R[[:space:]]+"\$SERVICE_USER:\$SERVICE_USER"[[:space:]]+/var/lib/evo' \
        "$BOOTSTRAP"; then
    fail "bootstrap still recursively chowns /var/lib/evo"
else
    pass "bootstrap has no chown -R of /var/lib/evo"
fi

if grep -q 'chown_tree_same_fs /var/lib/evo' "$BOOTSTRAP" \
    && grep -q 'lib/chown-tree-same-fs.sh' "$BOOTSTRAP"; then
    pass "bootstrap sources lib and calls chown_tree_same_fs"
else
    fail "bootstrap does not call chown_tree_same_fs on /var/lib/evo"
fi

ROOT="$(mktemp -d)"
cleanup() { rm -rf "$ROOT"; }
trap cleanup EXIT

if chown_tree_same_fs "$ROOT/missing" "$(id -u)"; then
    pass "missing tree is a no-op"
else
    fail "missing tree did not return 0"
fi

if unshare --user --map-root-user --mount true 2>/dev/null; then
    got="$(unshare --user --map-root-user --mount bash -c "
        set -euo pipefail
        # shellcheck disable=SC1091
        . \"$LIB_PATH\"
        root=\"$ROOT/var/lib/evo\"
        mkdir -p \"\$root/music/INTERNAL\" \"\$root/music/NAS/share\" \"\$root/state\"
        printf 'local\\n' > \"\$root/music/INTERNAL/a.flac\"
        printf 'nas\\n' > \"\$root/music/NAS/share/remote.flac\"
        printf 'state\\n' > \"\$root/state/x\"
        mount -t tmpfs -o ro tmpfs \"\$root/music/NAS/share\"
        nas_before=\$(stat -c %Z \"\$root/music/NAS/share\")
        if chown_tree_same_fs \"\$root\" 0 0; then
            rc=0
        else
            rc=\$?
        fi
        nas_after=\$(stat -c %Z \"\$root/music/NAS/share\")
        local_owner=\$(stat -c %u:%g \"\$root/music/INTERNAL/a.flac\")
        state_owner=\$(stat -c %u:%g \"\$root/state/x\")
        umount \"\$root/music/NAS/share\"
        printf '%s %s %s %s %s\\n' \"\$rc\" \"\$nas_before\" \"\$nas_after\" \"\$local_owner\" \"\$state_owner\"
    ")"
    rc="${got%% *}"
    rest="${got#* }"
    nas_before="${rest%% *}"
    rest="${rest#* }"
    nas_after="${rest%% *}"
    rest="${rest#* }"
    local_owner="${rest%% *}"
    state_owner="${rest##* }"
    if [[ "$rc" == "0" ]]; then
        pass "read-only NAS mount does not abort chown"
    else
        fail "chown aborted on read-only NAS mount (rc=$rc)"
    fi
    if [[ -n "$nas_before" && "$nas_before" == "$nas_after" ]]; then
        pass "NAS mount-point inode was not chowned"
    else
        fail "NAS mount-point inode was touched (before=$nas_before after=$nas_after)"
    fi
    if [[ "$local_owner" == "0:0" && "$state_owner" == "0:0" ]]; then
        pass "same-filesystem music and state were chowned"
    else
        fail "same-filesystem owners wrong (local=$local_owner state=$state_owner)"
    fi
else
    echo "SKIP  unshare mount not available — same-fs chown not live-exercised"
fi

echo "summary: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
