#!/usr/bin/env bash
# place-plugin-tool.test.sh — /opt/evo/bin/evo-plugin-tool from the
# signed 0.1.13.0 slot on the artefacts plane.
#
# place_plugin_tool fetches the binary with its .sha256 and .sig,
# checks the hash, then the signature against the commons plugin
# key in /etc/evo/trust.d/, and only then places the binary. A
# missing file, a missing key, a bad hash, a bad signature or an
# architecture with no slot is non-zero, and leaves no
# /opt/evo/bin/evo-plugin-tool and no temp file. install,
# reinstall and wipe-config run it after bootstrap has placed the
# key; wipe-user-data, which places no binaries, does not.
# deploy-distribution.sh runs the same function on the target,
# beside the steward.
#
# The function is extracted from the shipped installer with the
# expression deploy-distribution.sh uses, and fed to `bash -s` the
# way deploy feeds it to the target. Each case runs twice: under
# `set -euo pipefail`, as the installer calls it, and without, as
# deploy's `bash -s` runs it. The slot is a file:// tree signed
# for this run with a throwaway key, so nothing here reaches the
# network. Placement is root-owned, so the cases run as root in a
# user namespace, or directly when the test already runs as root.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
INSTALLER="$REPO_ROOT/dist/scripts/evo-install.sh"
DEPLOY="$REPO_ROOT/dist/scripts/deploy-distribution.sh"

PASS=0
FAIL=0
SKIP=0
ok()   { echo "PASS  $1"; PASS=$((PASS + 1)); }
bad()  { echo "FAIL  $1"; FAIL=$((FAIL + 1)); }
skip() { echo "SKIP  $1"; SKIP=$((SKIP + 1)); }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FN="$(sed -n '/^place_plugin_tool() {$/,/^}$/p' "$INSTALLER")"

# ----- the function as shipped -----

if [[ "$(head -n1 <<<"$FN")" == "place_plugin_tool() {" && "$(tail -n1 <<<"$FN")" == "}" ]]; then
    ok "the installer defines place_plugin_tool"
else
    bad "place_plugin_tool not found in the installer"
fi
if grep -qF 'https://raw.githubusercontent.com/foonerd/evo-device-audio-artefacts/8c75e6e/binaries/evo-plugin-tool/0.1.13.0}' <<<"$FN" \
    && ! grep -q '/main/' <<<"$FN"; then
    ok "the slot is pinned to the plane commit 8c75e6e, not main"
else
    bad "the slot URL is not the 8c75e6e pin"
fi
if grep -qF '/etc/evo/trust.d/commons-plugin-signing-public.pem' <<<"$FN"; then
    ok "the signature is checked against the commons plugin key"
else
    bad "the commons plugin key path is not in the function"
fi
hash_line="$(grep -n 'sha256sum' <<<"$FN" | head -n1 | cut -d: -f1)"
sig_line="$(grep -n 'pkeyutl -verify' <<<"$FN" | head -n1 | cut -d: -f1)"
if [[ -n "$hash_line" && -n "$sig_line" && "$hash_line" -lt "$sig_line" ]]; then
    ok "the hash is checked before the signature"
else
    bad "the hash is not checked before the signature"
fi
if grep -qE 'systemctl|evo\.sock|register|admin' <<<"$FN"; then
    bad "place_plugin_tool does more than place: $(grep -nE 'systemctl|evo\.sock|register|admin' <<<"$FN")"
else
    ok "place_plugin_tool starts nothing and registers nothing"
fi

# ----- where it runs -----

placed_after_bootstrap="invoke_bootstrap_placement ; write_installed_record ; TMPDIR=\"\${WORK_DIR}\" place_plugin_tool ;"
# The definition and the three calls; nothing else runs it.
if [[ "$(grep -cF "$placed_after_bootstrap" "$INSTALLER")" -eq 3 ]] \
    && [[ "$(grep -v '^[[:space:]]*#' "$INSTALLER" | grep -c 'place_plugin_tool')" -eq 4 ]]; then
    ok "the installer calls it three times, each after bootstrap placement"
else
    bad "the installer does not call it exactly three times after bootstrap placement"
fi
for mode in install reinstall wipe-config; do
    if awk "/^    ${mode}\\)/,/;;/" "$INSTALLER" | grep -qF "$placed_after_bootstrap"; then
        ok "$mode places evo-plugin-tool after bootstrap"
    else
        bad "$mode does not place evo-plugin-tool after bootstrap"
    fi
done
if awk '/^    wipe-user-data\)/,/;;/' "$INSTALLER" | grep -q place_plugin_tool; then
    bad "wipe-user-data places evo-plugin-tool, and it places no binaries"
else
    ok "wipe-user-data does not place evo-plugin-tool"
fi

if grep -qF "sed -n '/^place_plugin_tool() {\$/,/^}\$/p' \"\${SCRIPT_DIR}/evo-install.sh\"" "$DEPLOY" \
    && grep -qF "| ssh \"\${SSH_TARGET}\" 'sudo -n bash -s'" "$DEPLOY"; then
    ok "deploy ships the installer's own function to the target as root"
else
    bad "deploy does not ship the installer's place_plugin_tool to the target"
fi
steward_line="$(grep -n 'echo "  ok (steward installed)"' "$DEPLOY" | cut -d: -f1)"
tool_line="$(grep -n "| ssh \"\${SSH_TARGET}\" 'sudo -n bash -s'" "$DEPLOY" | cut -d: -f1)"
# The third loop over the plugins is the one that installs them.
plugins_line="$(grep -nxF "for entry in \"\${OOP_PLUGINS[@]}\"; do" "$DEPLOY" | sed -n 3p | cut -d: -f1)"
if [[ -n "$steward_line" && -n "$tool_line" && -n "$plugins_line" \
        && "$steward_line" -lt "$tool_line" && "$tool_line" -lt "$plugins_line" ]]; then
    ok "deploy places evo-plugin-tool right after the steward"
else
    bad "deploy does not place evo-plugin-tool beside the steward (steward=$steward_line tool=$tool_line plugins=$plugins_line)"
fi

# ----- the function, driven -----

if [[ "$EUID" -eq 0 ]]; then
    AS_ROOT=()
elif unshare --user --map-root-user true 2>/dev/null; then
    AS_ROOT=(unshare --user --map-root-user)
else
    skip "no root and no user namespace — placement cases not exercised"
    echo ""
    echo "place-plugin-tool.test.sh: $PASS passed, $FAIL failed, $SKIP skipped"
    [[ $FAIL -eq 0 ]]
    exit
fi

KEYS="$WORK/keys"
mkdir -p "$KEYS"
openssl genpkey -algorithm ed25519 -out "$KEYS/signing.pem" 2>/dev/null
openssl pkey -in "$KEYS/signing.pem" -pubout -out "$KEYS/public.pem"
openssl genpkey -algorithm ed25519 -out "$KEYS/other.pem" 2>/dev/null

# The slot's files, written the way publish-pieces.yml writes them.
sign_slot() {
    local dir="$1" key="${2:-$KEYS/signing.pem}"
    sha256sum "$dir/evo-plugin-tool" | cut -c1-64 > "$dir/evo-plugin-tool.sha256"
    openssl pkeyutl -sign -inkey "$key" -rawin \
        -in "$dir/evo-plugin-tool" -out "$dir/evo-plugin-tool.sig"
}
SLOT="$WORK/slot"
for triple in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu armv7-unknown-linux-gnueabihf; do
    mkdir -p "$SLOT/$triple"
    printf '#!/bin/sh\necho "evo-plugin-tool for %s"\n' "$triple" > "$SLOT/$triple/evo-plugin-tool"
    sign_slot "$SLOT/$triple"
done
X86="x86_64-unknown-linux-gnu"

# A fresh root holding /opt/evo/bin and the key, an empty TMPDIR,
# and a copy of the slot for the case to break.
N=0
CASE=""
fresh() {
    N=$((N + 1))
    CASE="$WORK/case-$N"
    mkdir -p "$CASE/root/opt/evo/bin" "$CASE/root/etc/evo/trust.d" "$CASE/tmp"
    cp "$KEYS/public.pem" "$CASE/root/etc/evo/trust.d/commons-plugin-signing-public.pem"
    cp -a "$SLOT" "$CASE/slot"
}

# What a directory holds, by name, on one line.
entries() { find "$1" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort | tr '\n' ' '; }

# Runs place_plugin_tool as root on $CASE through `bash -s`.
# Prints its exit code and the placed file's owner, or `none`.
run_place() {
    local context="$1" arch="$2" errexit=""
    if [[ "$context" == installer ]]; then
        errexit="set -euo pipefail"
    fi
    printf '%s\n%s\n' "$FN" "
uname() { if [[ \"\${1:-}\" == -m ]]; then echo \"\$FAKE_ARCH\"; else command uname \"\$@\"; fi; }
( ${errexit:-:} ; place_plugin_tool \"\$ROOT\" \"\$BASE\" ) >/dev/null 2>\"\$ERR\"
rc=\$?
owner=\"\$(stat -c %u:%g \"\$ROOT/opt/evo/bin/evo-plugin-tool\" 2>/dev/null || echo none)\"
echo \"\$rc \$owner\"" \
        | FAKE_ARCH="$arch" ROOT="$CASE/root" BASE="file://$CASE/slot" \
            TMPDIR="$CASE/tmp" ERR="$CASE/err" "${AS_ROOT[@]}" bash -s
}

# assert_placed <name> <uname -m> <triple> [setup]
assert_placed() {
    local name="$1" arch="$2" triple="$3" setup="${4:-:}" context out rc owner
    for context in installer deploy; do
        fresh
        "$setup"
        out="$(run_place "$context" "$arch")"
        rc="${out%% *}"
        owner="${out#* }"
        if [[ "$rc" == 0 && "$owner" == "0:0" ]] \
            && cmp -s "$CASE/root/opt/evo/bin/evo-plugin-tool" "$SLOT/$triple/evo-plugin-tool" \
            && [[ "$(stat -c %a "$CASE/root/opt/evo/bin/evo-plugin-tool")" == 755 ]] \
            && [[ "$(entries "$CASE/root/opt/evo/bin")" == "evo-plugin-tool " ]] \
            && [[ -z "$(entries "$CASE/tmp")" ]]; then
            ok "$name ($context)"
        else
            bad "$name ($context): rc=$rc owner=$owner bin=[$(entries "$CASE/root/opt/evo/bin")] tmp=[$(entries "$CASE/tmp")] err=$(cat "$CASE/err")"
        fi
    done
}

# assert_refused <name> <uname -m> <setup> <exit> <stderr pattern>
# The exit codes are the installer's: 1 a missing prerequisite,
# 2 a failed fetch, 3 a failed verification, 4 a failed apply.
assert_refused() {
    local name="$1" arch="$2" setup="$3" want="$4" pattern="$5" context out rc owner
    for context in installer deploy; do
        fresh
        "$setup"
        out="$(run_place "$context" "$arch")"
        rc="${out%% *}"
        owner="${out#* }"
        if [[ "$rc" == "$want" && "$owner" == none ]] \
            && [[ -z "$(entries "$CASE/root/opt/evo/bin")" ]] \
            && [[ -z "$(entries "$CASE/tmp")" ]] \
            && grep -q -- "$pattern" "$CASE/err"; then
            ok "$name ($context, exit $rc)"
        else
            bad "$name ($context): rc=$rc owner=$owner bin=[$(entries "$CASE/root/opt/evo/bin")] tmp=[$(entries "$CASE/tmp")] err=$(cat "$CASE/err")"
        fi
    done
}

no_binary()   { rm "$CASE/slot/$X86/evo-plugin-tool"; }
no_sha256()   { rm "$CASE/slot/$X86/evo-plugin-tool.sha256"; }
no_sig()      { rm "$CASE/slot/$X86/evo-plugin-tool.sig"; }
no_key()      { rm "$CASE/root/etc/evo/trust.d/commons-plugin-signing-public.pem"; }
wrong_hash()  { printf 'not the tool' | sha256sum | cut -c1-64 > "$CASE/slot/$X86/evo-plugin-tool.sha256"; }
empty_hash()  { : > "$CASE/slot/$X86/evo-plugin-tool.sha256"; }
# Both stale: the hash is the check that refuses.
swapped_binary() { printf 'another tool\n' > "$CASE/slot/$X86/evo-plugin-tool"; }
# The hash is made to match; only the signature can refuse.
swapped_binary_and_hash() {
    printf 'another tool\n' > "$CASE/slot/$X86/evo-plugin-tool"
    sha256sum "$CASE/slot/$X86/evo-plugin-tool" | cut -c1-64 > "$CASE/slot/$X86/evo-plugin-tool.sha256"
}
other_key()   { sign_slot "$CASE/slot/$X86" "$KEYS/other.pem"; }
stale_stage() { printf 'killed run\n' > "$CASE/root/opt/evo/bin/.evo-plugin-tool.AbC123"; }
earlier_tool() { printf 'earlier tool\n' > "$CASE/root/opt/evo/bin/evo-plugin-tool"; }

assert_placed "x86_64 places the x86_64 slot" x86_64 x86_64-unknown-linux-gnu
assert_placed "aarch64 places the aarch64 slot" aarch64 aarch64-unknown-linux-gnu
assert_placed "armv7l places the armv7 slot" armv7l armv7-unknown-linux-gnueabihf
assert_placed "a verified tool replaces an earlier one" x86_64 "$X86" earlier_tool
assert_placed "a staged copy left by a killed run is removed" x86_64 "$X86" stale_stage

assert_refused "an architecture with no slot refuses" i686 : 1 "no evo-plugin-tool for architecture i686"
assert_refused "armv7 without the l is not armv7l" armv7 : 1 "no evo-plugin-tool for architecture armv7"
assert_refused "a missing binary refuses" x86_64 no_binary 2 "fetch .*/evo-plugin-tool failed"
assert_refused "a missing .sha256 refuses" x86_64 no_sha256 2 "fetch .*/evo-plugin-tool.sha256 failed"
assert_refused "a missing .sig refuses" x86_64 no_sig 2 "fetch .*/evo-plugin-tool.sig failed"
assert_refused "a missing key refuses" x86_64 no_key 4 "commons plugin key missing"
assert_refused "a .sha256 for other bytes refuses" x86_64 wrong_hash 3 "sha256 is"
assert_refused "an empty .sha256 refuses" x86_64 empty_hash 3 "says nothing"
assert_refused "a swapped binary is refused by its hash first" x86_64 swapped_binary 3 "sha256 is"
assert_refused "a swapped binary with a matching hash is refused by its signature" x86_64 swapped_binary_and_hash 3 "signature does not verify"
assert_refused "a signature by another key refuses" x86_64 other_key 3 "signature does not verify"

# A refusal does not touch the tool already in place.
for context in installer deploy; do
    fresh
    earlier_tool
    other_key
    out="$(run_place "$context" x86_64)"
    if [[ "${out%% *}" == 3 && "$(cat "$CASE/root/opt/evo/bin/evo-plugin-tool")" == "earlier tool" ]] \
        && [[ "$(entries "$CASE/root/opt/evo/bin")" == "evo-plugin-tool " ]] \
        && [[ -z "$(entries "$CASE/tmp")" ]]; then
        ok "a refusal leaves an earlier tool as it was ($context)"
    else
        bad "a refusal changed what was in place ($context): $out bin=[$(entries "$CASE/root/opt/evo/bin")]"
    fi
done

echo ""
echo "place-plugin-tool.test.sh: $PASS passed, $FAIL failed, $SKIP skipped"
[[ $FAIL -eq 0 ]]
