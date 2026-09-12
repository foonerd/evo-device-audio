#!/usr/bin/env bash
# install-modes-doc.test.sh — the operator-facing mode table must
# describe what the installer actually does.
#
# The class this exists for: INSTALL.md told operators that
# `--mode=reinstall` "Preserves the music library at
# /var/lib/evo/music". wipe_full runs `rm -rf /var/lib/evo`. The
# installer's own header has always said NOTHING survives that
# wipe, including the music library. Two truths, one command.
#
# An operator who hits a failed install, trusts the table, and
# reaches for reinstall to "start over safely" loses the library.
# That is how a false FAIL becomes a brick — which is exactly the
# path the journal-classifier row closed at the other end.
#
# Both sides are read from the shipped files: the prose from
# INSTALL.md and the behaviour from evo-install.sh. Nothing is
# copied into a fixture, so the doc cannot drift back.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
INSTALLER="$REPO_ROOT/dist/scripts/evo-install.sh"
DOC="$REPO_ROOT/INSTALL.md"

PASS=0
FAIL=0
ok()  { echo "PASS  $1"; PASS=$((PASS + 1)); }
bad() { echo "FAIL  $1"; FAIL=$((FAIL + 1)); }

[[ -r "$DOC" ]]       || { echo "FAIL  INSTALL.md not readable at $DOC" >&2; exit 1; }
[[ -r "$INSTALLER" ]] || { echo "FAIL  evo-install.sh not readable" >&2; exit 1; }

# --- the behaviour half: read it out of the code ---
WIPE_FULL="$(awk '/^wipe_full\(\) \{/{on=1} on{print} on&&/^\}$/{exit}' "$INSTALLER")"
if printf '%s\n' "$WIPE_FULL" | grep -qE '^\s*rm -rf /var/lib/evo\s*$'; then
    ok "code: wipe_full removes /var/lib/evo (the library lives there)"
else
    bad "code: wipe_full no longer removes /var/lib/evo — this test's premise changed, re-read before editing prose"
fi

# --- the operator table: the reinstall row ---
REINSTALL_ROW="$(grep -n '^| *`reinstall`' "$DOC" || true)"
if [[ -n "$REINSTALL_ROW" ]]; then
    ok "doc: the mode table has a reinstall row"
else
    bad "doc: no reinstall row found — the table moved; fix this test, not by deleting the case"
fi

if printf '%s\n' "$REINSTALL_ROW" | grep -qiE 'preserv|keeps? the music|music library (is )?(kept|retained|untouched)'; then
    bad "doc: reinstall row still promises the music library survives — wipe_full deletes it"
else
    ok "doc: reinstall row makes no preservation promise"
fi

# Deletion and the library must be the SAME claim. "Deletes prior
# state." followed by "Preserves the music library." satisfies a
# naive two-part check while saying the opposite of the truth —
# that is precisely the sentence this row exists to kill.
if printf '%s\n' "$REINSTALL_ROW" | grep -qiE '(delet|remov|destroy)[a-z]*[^.|]{0,80}music'; then
    ok "doc: reinstall row states, in one clause, that the music library is deleted"
else
    bad "doc: reinstall row never says the music library itself is deleted"
fi

# --- wipe-config must remain the preserve-music primitive ---
WIPECONF_ROW="$(grep -n '^| *`wipe-config`' "$DOC" || true)"
if printf '%s\n' "$WIPECONF_ROW" | grep -qiE 'keeps? the music|music library.*(untouched|preserv)|preserv.*music'; then
    ok "doc: wipe-config remains the row that keeps the music library"
else
    bad "doc: wipe-config no longer promises the music library is kept"
fi

# --- one story: the installer header says the same thing ---
HEADER="$(sed -n '1,60p' "$INSTALLER")"
if printf '%s\n' "$HEADER" | grep -qi 'including the music'; then
    ok "code header: reinstall is documented as taking the music library too"
else
    bad "code header: reinstall no longer says the music library goes with it"
fi

# --- no other operator-facing table may carry the old promise ---
OTHERS=0
while IFS= read -r f; do
    [[ "$f" == "$DOC" ]] && continue
    if grep -qE '^\| *`reinstall`' "$f" 2>/dev/null; then
        if grep -E '^\| *`reinstall`' "$f" | grep -qiE 'preserv|keeps? the music'; then
            bad "doc: $f also promises reinstall preserves music"
            OTHERS=$((OTHERS + 1))
        fi
    fi
done < <(find "$REPO_ROOT" -name '*.md' -not -path '*/target/*' -not -path '*/node_modules/*')
[[ "$OTHERS" -eq 0 ]] && ok "doc: no other operator-facing table repeats the promise"

echo ""
echo "install-modes-doc.test.sh: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
