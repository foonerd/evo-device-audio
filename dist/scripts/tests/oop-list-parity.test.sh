#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright (c) 2026 Just a Nerd
#
# oop-list-parity.test.sh — the ship list and the plugins tree say the same thing.
#
# dist/scripts/lib/oop-plugins.sh is the one list the installer bundle, the
# redeploy script and the published pieces are built from, and the redeploy
# script removes every plugin directory on a box that is not on it. A plugin
# that exists in the tree and is missing from the list is therefore never
# shipped by those paths and is deleted by a redeploy. This test makes that
# drift fail here, offline, instead of on a device:
#
#   1. every plugin that has an out-of-process manifest is on the list, or
#      on the staged list of plugins deliberately not shipped yet (which the
#      bundle, the redeploy and the published pieces never read);
#   2. every entry of either list has its manifest, privileges file and
#      crate, with the crate name, the wire binary and any feature spelled as
#      the crate declares them;
#   3. a crate that declares the ALSA device feature is built with it;
#   4. no plugin is on both lists, or twice;
#   5. the expected-count constant is the shipped list's length.
#
# It reads only the repository; it needs python3 (3.11 or newer) for TOML.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
LIST="${OOP_LIST_UNDER_TEST:-$REPO_ROOT/dist/scripts/lib/oop-plugins.sh}"
PLUGINS_DIR="${OOP_PLUGINS_DIR_UNDER_TEST:-$REPO_ROOT/plugins}"

PASS=0
FAIL=0
ok()  { echo "PASS  $1"; PASS=$((PASS + 1)); }
bad() { echo "FAIL  $1"; FAIL=$((FAIL + 1)); }

# shellcheck source=../lib/oop-plugins.sh
source "$LIST"

# Staged plugins: in the tree, deliberately not shipped. May be absent.
STAGED=("${OOP_PLUGINS_STAGED[@]+"${OOP_PLUGINS_STAGED[@]}"}")
[[ -n "${STAGED[*]-}" ]] || STAGED=()

# Names on the shipped list, and on either list, one per line.
shipped="$(for e in "${OOP_PLUGINS[@]}"; do echo "${e%%:*}"; done | sort)"
staged="$(for e in "${STAGED[@]+"${STAGED[@]}"}"; do [[ -n "$e" ]] && echo "${e%%:*}"; done | sort)"
listed="$(printf '%s\n%s\n' "$shipped" "$staged" | grep . | sort)"
# Plugins in the tree that ship out of process.
in_tree="$(cd "$PLUGINS_DIR" && for d in */; do
    d="${d%/}"
    [[ -f "$d/manifest.oop.toml" ]] && echo "$d"
done | sort)"

# 1. nothing in the tree is missing from the list
missing="$(comm -13 <(echo "$listed") <(echo "$in_tree"))"
if [[ -z "$missing" ]]; then
    ok "every plugin with an out-of-process manifest is on the ship list or the staged list"
else
    bad "plugins in the tree but on neither list (never shipped, deleted by a redeploy): $(echo "$missing" | tr '\n' ' ')"
fi

# 2a. nothing on the list is missing from the tree
extra="$(comm -23 <(echo "$listed") <(echo "$in_tree"))"
if [[ -z "$extra" ]]; then
    ok "every listed entry has an out-of-process manifest in the tree"
else
    bad "listed entries with no manifest.oop.toml: $(echo "$extra" | tr '\n' ' ')"
fi

# no name twice
dups="$(echo "$listed" | uniq -d)"
if [[ -z "$dups" ]]; then
    ok "no plugin is on the lists twice, or on both"
else
    bad "plugins listed twice, or on both the ship list and the staged list: $(echo "$dups" | tr '\n' ' ')"
fi

# 2b/3. each entry's crate, wire binary, feature and privileges file
report="$(python3 - "$PLUGINS_DIR" "${OOP_PLUGINS[@]}" "${STAGED[@]+"${STAGED[@]}"}" <<'PY'
import sys, tomllib, os
root, entries = sys.argv[1], sys.argv[2:]
for e in entries:
    parts = e.split(":")
    if len(parts) != 4:
        print(f"{e}: not <name>:<crate>:<wire>:<features>"); continue
    name, crate, wire, feat = parts
    d = os.path.join(root, name)
    cargo = os.path.join(d, "Cargo.toml")
    if not os.path.isfile(cargo):
        print(f"{name}: no Cargo.toml"); continue
    c = tomllib.load(open(cargo, "rb"))
    if c.get("package", {}).get("name") != crate:
        print(f"{name}: list says crate {crate}, Cargo.toml says {c.get('package', {}).get('name')}")
    bins = [b.get("name") for b in c.get("bin", [])]
    if wire not in bins:
        print(f"{name}: wire binary {wire} is not a [[bin]] of the crate ({bins})")
    declared = set(c.get("features", {}))
    for f in [x for x in feat.split(",") if x]:
        if f not in declared:
            print(f"{name}: feature {f} is not declared by the crate")
    if "alsa-substrate" in declared and "alsa-substrate" not in feat.split(","):
        print(f"{name}: the crate declares alsa-substrate but the list builds it without")
    if not os.path.isfile(os.path.join(d, "privileges.yaml")):
        print(f"{name}: no privileges.yaml")
PY
)"
if [[ -z "$report" ]]; then
    ok "each entry's crate, wire binary, feature and privileges file match the crate"
else
    bad "entry does not match its crate: $(echo "$report" | tr '\n' ';')"
fi

# 5. the expected count is the shipped list; the tree is shipped plus staged
n_staged="$(echo "$staged" | grep -c . || true)"
if [[ "$OOP_PLUGINS_EXPECTED_COUNT" -eq "${#OOP_PLUGINS[@]}" ]] \
    && [[ "$(( OOP_PLUGINS_EXPECTED_COUNT + n_staged ))" -eq "$(echo "$in_tree" | grep -c .)" ]]; then
    ok "the expected-count constant (${OOP_PLUGINS_EXPECTED_COUNT}) is the length of the ship list, and with the ${n_staged} staged it is the number of plugins in the tree"
else
    bad "expected count ${OOP_PLUGINS_EXPECTED_COUNT}, ship list ${#OOP_PLUGINS[@]}, staged ${n_staged}, tree $(echo "$in_tree" | grep -c .)"
fi

echo ""
echo "oop-list-parity.test.sh: $PASS passed, $FAIL failed"
[[ "$FAIL" -eq 0 ]]
