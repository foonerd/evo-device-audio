#!/usr/bin/env bash
# music-hash-snapshot.test.sh — wipe-config / wipe-user-data
# hash the local music filesystem only. USB and NAS adopts are
# other filesystems and must not enter the digest.
#
# Extracts snapshot_music_hashes from evo-install.sh so the
# test cannot drift from the shipped function.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALLER="$(cd "$SCRIPT_DIR/.." && pwd)/evo-install.sh"

PASS=0
FAIL=0

pass() { echo "PASS  $*"; PASS=$((PASS + 1)); }
fail() { echo "FAIL  $*"; FAIL=$((FAIL + 1)); }

# Structural lock: the production find must not walk mounts.
if grep -q 'find "${root}" -xdev -type f' "$INSTALLER" \
    && grep -q 'find "${root}" -xdev -type f -print0' "$INSTALLER"; then
    pass "find uses -xdev"
else
    fail "snapshot_music_hashes find is missing -xdev"
fi

FUNC="$(awk '/^snapshot_music_hashes\(\)/,/^}$/' "$INSTALLER")"
if [[ -z "$FUNC" ]]; then
    echo "FAIL  could not extract snapshot_music_hashes" >&2
    exit 1
fi
eval "$FUNC"

ROOT="$(mktemp -d)"
cleanup() { rm -rf "$ROOT"; }
trap cleanup EXIT

mkdir -p "$ROOT/music/INTERNAL" "$ROOT/music/NAS" "$ROOT/music/USB"
printf 'local-a\n' > "$ROOT/music/INTERNAL/a.flac"
printf 'local-b\n' > "$ROOT/music/INTERNAL/b.flac"
printf 'should-not-count-if-mounted\n' > "$ROOT/music/NAS/remote.flac"

h1="$(snapshot_music_hashes "$ROOT/music")"
h2="$(snapshot_music_hashes "$ROOT/music")"
if [[ -n "$h1" && "$h1" == "$h2" && "$h1" != "no_music_library" ]]; then
    pass "same-fs digest is stable ($h1)"
else
    fail "same-fs digest unstable or empty (h1=$h1 h2=$h2)"
fi

# Adding a file on the same filesystem must change the digest.
printf 'local-c\n' > "$ROOT/music/INTERNAL/c.flac"
h3="$(snapshot_music_hashes "$ROOT/music")"
if [[ "$h3" != "$h1" ]]; then
    pass "local add changes digest"
else
    fail "local add did not change digest"
fi

if snapshot_music_hashes "$ROOT/no-such-music" | grep -qx 'no_music_library'; then
    pass "missing root prints no_music_library"
else
    fail "missing root did not print no_music_library"
fi

# Foreign filesystem (NAS adopt): must not enter the digest.
if unshare --user --map-root-user --mount true 2>/dev/null; then
    got="$(unshare --user --map-root-user --mount bash -c "
        set -euo pipefail
        $(printf '%s\n' "$FUNC")
        root=\"$ROOT/music\"
        # Drop the same-fs placeholder under NAS, then mount over it.
        rm -f \"\$root/NAS/remote.flac\"
        mount -t tmpfs tmpfs \"\$root/NAS\"
        printf 'nas-only\\n' > \"\$root/NAS/remote.flac\"
        before=\$(snapshot_music_hashes \"\$root\")
        printf 'nas-changed\\n' > \"\$root/NAS/remote.flac\"
        after=\$(snapshot_music_hashes \"\$root\")
        umount \"\$root/NAS\"
        printf '%s %s\\n' \"\$before\" \"\$after\"
    ")"
    before="${got%% *}"
    after="${got##* }"
    if [[ -n "$before" && "$before" == "$after" ]]; then
        pass "NAS tmpfs writes do not change digest"
    else
        fail "NAS tmpfs writes leaked into digest (before=$before after=$after)"
    fi
else
    echo "SKIP  unshare mount not available — -xdev behaviour not live-exercised"
fi

echo "summary: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
