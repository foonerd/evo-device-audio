#!/usr/bin/env bash
# installed-release-record.test.sh — the installed release the
# system.info plugin reads from /var/lib/evo/installed.toml.
#
# Both bundle builders stamp `release` into bundle-manifest.toml
# from CUT_VERSION: the one line, without a leading v, and never
# the Cargo version. evo-install.sh keeps `release`,
# `architecture` and `built_at_utc` from the staged manifest after
# the wipe, at mode 0644. It copies no `version` and invents no
# release, and no later step of the install deletes the file.
#
# The functions under test are extracted from the shipped scripts
# at test time, so the test cannot drift from the files it guards.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
BUILD="$REPO_ROOT/dist/scripts/build-bundle.sh"
COMPOSE="$REPO_ROOT/scripts/release/compose-distribution-from-pieces.sh"
INSTALLER="$REPO_ROOT/dist/scripts/evo-install.sh"

PASS=0
FAIL=0
ok() { echo "PASS  $1"; PASS=$((PASS + 1)); }
bad() { echo "FAIL  $1"; FAIL=$((FAIL + 1)); }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# A shell function's text, from its `name() {` line to its `}`.
extract_fn() { sed -n "/^$2() {/,/^}/p" "$1"; }

# ----- the builders read the release from CUT_VERSION -----

eval "$(extract_fn "$BUILD" read_cut_release)"
cut_case() {
    local name="$1" body="$2" expect="$3" got rc
    local root="$WORK/cut-$PASS-$FAIL"
    mkdir -p "$root"
    if [[ "$body" != "<absent>" ]]; then printf '%b' "$body" > "$root/CUT_VERSION"; fi
    rc=0
    got="$(read_cut_release "$root" 2>/dev/null)" || rc=$?
    if [[ "$expect" == "<refused>" ]]; then
        if [[ $rc -ne 0 ]]; then ok "$name"; else bad "$name (read: $got)"; fi
    elif [[ $rc -eq 0 && "$got" == "$expect" ]]; then
        ok "$name"
    else
        bad "$name (rc=$rc, read: $got)"
    fi
}
cut_case "a missing CUT_VERSION refuses the build" "<absent>" "<refused>"
cut_case "an empty CUT_VERSION refuses the build" "\n" "<refused>"
cut_case "a leading v refuses the build" "v0.1.13.1\n" "<refused>"
cut_case "the one line is the release" "0.1.13.1\n" "0.1.13.1"

if grep -q 'echo "release = \\"${CUT_RELEASE}\\""' "$BUILD" \
    && grep -q 'echo "release = \\"${CUT_VER}\\""' "$COMPOSE"; then
    ok "both builders stamp release from the cut"
else
    bad "a builder does not stamp release from the cut"
fi
if grep -E 'release = .*(DIST_VERSION|CARGO_VERSION)' "$BUILD" "$COMPOSE"; then
    bad "a builder stamps release from the Cargo version"
else
    ok "no builder stamps release from the Cargo version"
fi
if grep -q 'BUNDLE_BASE="evo-device-audio-${TARGET_TRIPLE}-${DIST_VERSION}"' "$BUILD"; then
    ok "the tarball name stays the three-part Cargo version"
else
    bad "the tarball name moved off the Cargo version"
fi

# ----- the installer keeps the record -----

eval "$(extract_fn "$INSTALLER" manifest_string)"
eval "$(extract_fn "$INSTALLER" write_installed_record)"

manifest() {
    local path="$WORK/$1.toml"
    printf '%b' "$2" > "$path"
    printf '%s' "$path"
}

stamped="$(manifest stamped 'schema_version = 1\nbundle_kind = "evo-device-audio"\nversion = "0.1.13"\nrelease = "0.1.13.1"\narchitecture = "aarch64-unknown-linux-gnu"\ncomposed_from_pieces = true\nbuilt_at_utc = "2026-10-28T09:00:00Z"\n\n[plugins]\norg.evoframework.alarm = true\n')"
out="$WORK/var/lib/evo/installed.toml"
write_installed_record "$stamped" "$out" >/dev/null
want='release = "0.1.13.1"
architecture = "aarch64-unknown-linux-gnu"
built_at_utc = "2026-10-28T09:00:00Z"'
if [[ "$(cat "$out")" == "$want" ]]; then
    ok "the record keeps release, architecture and built_at_utc"
else
    bad "the record is: $(cat "$out")"
fi
if [[ "$(stat -c %a "$out")" == "644" ]]; then
    ok "the record is mode 0644"
else
    bad "the record is mode $(stat -c %a "$out")"
fi
if grep -q '^version' "$out"; then
    bad "the record copied version"
else
    ok "the record copies no version"
fi

unstamped="$(manifest unstamped 'schema_version = 1\nversion = "0.1.13"\narchitecture = "x86_64-unknown-linux-gnu"\nbuilt_at_utc = "2026-10-28T09:00:00Z"\n\n[plugins]\n')"
write_installed_record "$unstamped" "$out" >/dev/null
if grep -q '^release' "$out"; then
    bad "a manifest with no release got one: $(grep '^release' "$out")"
else
    ok "a manifest with no release gets none; release is not the Cargo version"
fi
if [[ "$(grep -c '' "$out")" -eq 2 ]]; then
    ok "a rewrite replaces the record whole"
else
    bad "the rewritten record is: $(cat "$out")"
fi

odd="$(manifest odd 'version = "0.1.13"\nrelease = ""\n\n[plugins]\nrelease = "0.9"\n')"
write_installed_record "$odd" "$out" >/dev/null
if [[ ! -s "$out" ]]; then
    ok "an empty release, and a release under a table, are not taken"
else
    bad "the record from an odd manifest is: $(cat "$out")"
fi
leftovers=("$WORK/var/lib/evo/installed.toml."*)
if [[ -e "${leftovers[0]}" ]]; then
    bad "a temporary file was left beside the record"
else
    ok "no temporary file is left beside the record"
fi

# ----- where the install writes it -----

if [[ "$(grep -c 'invoke_bootstrap_placement ; write_installed_record ;' "$INSTALLER")" -eq 3 ]]; then
    ok "install, reinstall and wipe-config write the record after bootstrap"
else
    bad "the record is not written after bootstrap in exactly three modes"
fi
if awk '/^    wipe-user-data\)/,/;;/' "$INSTALLER" | grep -q write_installed_record; then
    bad "wipe-user-data writes a record for binaries it does not install"
else
    ok "wipe-user-data keeps the record of the binaries it keeps"
fi
if awk '/^    reinstall\)/,/;;/' "$INSTALLER" | grep -n 'wipe_full\|write_installed_record' \
    | awk -F: 'NR==1 && /wipe_full/ { w=1 } NR==2 && /write_installed_record/ && w { ok=1 } END { exit !ok }' \
    && awk '/^    wipe-config\)/,/;;/' "$INSTALLER" | grep -n 'wipe_config ;\|write_installed_record' \
    | awk -F: 'NR==1 && /wipe_config/ { w=1 } NR==2 && /write_installed_record/ && w { ok=1 } END { exit !ok }'; then
    ok "the record is written after the wipe"
else
    bad "the record is not written after the wipe"
fi
removers="$(awk '
    /^[a-z_]+\(\) \{/ { fn = $1; sub(/\(\)/, "", fn) }
    /^}/ { fn = "" }
    /rm -[a-z]* .*\/var\/lib\/evo|find \/var\/lib\/evo.*rm / { print (fn == "" ? "<top>" : fn) }
' "$INSTALLER" | sort -u | tr '\n' ' ')"
if [[ "$removers" == "wipe_config wipe_full wipe_user_data " ]] \
    && ! extract_fn "$INSTALLER" wipe_user_data | grep -q 'installed.toml\|rm -rf /var/lib/evo$'; then
    ok "only the wipes remove under /var/lib/evo, and none after the record"
else
    bad "something besides the wipes removes under /var/lib/evo: $removers"
fi

echo ""
echo "installed-release-record.test.sh: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
