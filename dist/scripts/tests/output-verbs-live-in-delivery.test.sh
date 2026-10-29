#!/usr/bin/env bash
# output-verbs-live-in-delivery.test.sh — the output verbs
# belong to the delivery plugin and nowhere else.
#
# `delivery.enable_output`, `delivery.disable_output`,
# `delivery.set_output_volume`, `delivery.play_output` and
# `delivery.pause_output` act on one ALSA output each;
# `delivery.add_logical_output`, `delivery.update_logical_output`
# and `delivery.remove_logical_output` name one, and
# `delivery.list_logical_outputs` reads the names back;
# `zone.add`, `zone.update`, `zone.remove` and `zone.list` keep a
# name over one or more logical outputs, and `zone.set_eq_engaged`
# and `zone.set_eq_band` keep a zone's own EQ; `scene.save`,
# `scene.list`, `scene.recall` and `scene.remove` keep a snapshot
# of every zone under a name. The delivery
# plugin owns the chain definition those verbs steer and the
# names the operator gives its outputs; no steward crate and no
# other plugin carries an output policy, a second copy of a
# verb, or a parallel truth about which output is playing,
# what it is called, or which zone it is in. This test goes red the moment one of the
# names appears in a Rust source, a manifest or a privileges
# file outside `plugins/org.evoframework.delivery.alsa`.
#
# One caller is named: `plugins/org.evoframework.system.backup`
# reads and puts back logical outputs and zones by calling the
# owner's verbs through the shelf dispatcher, so it may name
# them. It may not carry them: the test goes red if either of its
# manifests stocks one as a request type, or if its `src/lib.rs`
# handles one as its own request type.
#
# Documentation and this test directory are not scanned: a
# changelog may name a verb; a crate may not.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

OWNER="plugins/org.evoframework.delivery.alsa"
# The only plugin that may name the verbs besides their owner, and
# only to call them.
CALLER="plugins/org.evoframework.system.backup"
VERBS=(
    "delivery.enable_output"
    "delivery.disable_output"
    "delivery.set_output_volume"
    "delivery.play_output"
    "delivery.pause_output"
    "delivery.add_logical_output"
    "delivery.update_logical_output"
    "delivery.remove_logical_output"
    "delivery.list_logical_outputs"
    "zone.add"
    "zone.update"
    "zone.remove"
    "zone.list"
    "zone.set_eq_engaged"
    "zone.set_eq_band"
    "scene.save"
    "scene.list"
    "scene.recall"
    "scene.remove"
)

PASS=0
FAIL=0

# Where a verb may be named: the owning plugin and the one named
# caller. Everything else under crates/ and plugins/ (Rust
# sources, manifests, privileges) must not name it.
#
# A verb is named when it appears as a whole name: the character
# before it is not one that can continue a verb name (a letter, a
# digit, `_` or `.`), and neither is the character after it (a
# letter, a digit or `_`). So `timezone.list` and `zone.list_all`
# do not name `zone.list`; `"zone.list"`, `zone.list` on its own
# and `zone.list,` do.
scan_for() {
    local verb="$1"
    local verb_re="${verb//./\\.}"
    # grep -r exits 1 on no match; that is the pass path.
    ( cd "$REPO_DIR" && grep -rlE --include='*.rs' --include='*.toml' \
        --include='*.yaml' --include='*.yml' \
        "(^|[^A-Za-z0-9_.])${verb_re}([^A-Za-z0-9_]|\$)" crates plugins \
        2>/dev/null || true ) \
        | { grep -v -e "^$OWNER/" -e "^$CALLER/" || true; }
}

for verb in "${VERBS[@]}"; do
    hits="$(scan_for "$verb")"
    if [[ -z "$hits" ]]; then
        echo "PASS  $verb is named only under $OWNER and its caller"
        PASS=$((PASS + 1))
    else
        echo "FAIL  $verb is named outside $OWNER and its caller:"
        printf '      %s\n' $hits
        FAIL=$((FAIL + 1))
    fi
done

# The caller names the verbs to call them, never to carry them:
# neither of its manifests stocks one as a request type, and its
# lib.rs has no match arm that handles one.
stocked_by_caller() {
    local verb="$1" manifest
    for manifest in "$REPO_DIR/$CALLER/manifest.toml" "$REPO_DIR/$CALLER/manifest.oop.toml"; do
        if awk '/request_types[[:space:]]*=[[:space:]]*\[/ { f = 1 } f { print } f && /\]/ { f = 0 }' \
            "$manifest" | grep -q -F "\"$verb\""; then
            return 0
        fi
    done
    return 1
}
handled_by_caller() {
    local verb_re="${1//./\\.}"
    grep -Eq "\"$verb_re\"[[:space:]]*=>" "$REPO_DIR/$CALLER/src/lib.rs"
}
for verb in "${VERBS[@]}"; do
    if stocked_by_caller "$verb"; then
        echo "FAIL  $verb is stocked by $CALLER"
        FAIL=$((FAIL + 1))
    elif handled_by_caller "$verb"; then
        echo "FAIL  $verb is handled as a request type by $CALLER"
        FAIL=$((FAIL + 1))
    else
        echo "PASS  $verb is called, not carried, by $CALLER"
        PASS=$((PASS + 1))
    fi
done

# The owner must actually carry each verb, or the scan above is
# vacuous: a renamed verb would pass with nobody owning it.
for verb in "${VERBS[@]}"; do
    if ( cd "$REPO_DIR" && grep -rq -F "\"$verb\"" "$OWNER/src" "$OWNER/manifest.toml" "$OWNER/manifest.oop.toml" ); then
        echo "PASS  $verb is carried by $OWNER"
        PASS=$((PASS + 1))
    else
        echo "FAIL  $verb is not carried by $OWNER"
        FAIL=$((FAIL + 1))
    fi
done

echo "output-verbs-live-in-delivery: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
