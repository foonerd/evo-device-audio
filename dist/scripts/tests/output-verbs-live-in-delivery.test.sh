#!/usr/bin/env bash
# output-verbs-live-in-delivery.test.sh — the five output verbs
# belong to the delivery plugin and nowhere else.
#
# `delivery.enable_output`, `delivery.disable_output`,
# `delivery.set_output_volume`, `delivery.play_output` and
# `delivery.pause_output` act on one ALSA output each. The
# delivery plugin owns the chain definition those verbs steer;
# no steward crate and no other plugin carries an output
# policy, a second copy of a verb, or a parallel truth about
# which output is playing. This test goes red the moment one of
# the five names appears in a Rust source, a manifest or a
# privileges file outside `plugins/org.evoframework.delivery.alsa`.
#
# Documentation and this test directory are not scanned: a
# changelog may name a verb; a crate may not.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"

OWNER="plugins/org.evoframework.delivery.alsa"
VERBS=(
    "delivery.enable_output"
    "delivery.disable_output"
    "delivery.set_output_volume"
    "delivery.play_output"
    "delivery.pause_output"
)

PASS=0
FAIL=0

# Where a verb may be named: the owning plugin. Everything else
# under crates/ and plugins/ (Rust sources, manifests,
# privileges) must not name it.
scan_for() {
    local verb="$1"
    # grep -r exits 1 on no match; that is the pass path.
    ( cd "$REPO_DIR" && grep -rl --include='*.rs' --include='*.toml' \
        --include='*.yaml' --include='*.yml' -F "$verb" crates plugins \
        2>/dev/null || true ) \
        | { grep -v "^$OWNER/" || true; }
}

for verb in "${VERBS[@]}"; do
    hits="$(scan_for "$verb")"
    if [[ -z "$hits" ]]; then
        echo "PASS  $verb is named only under $OWNER"
        PASS=$((PASS + 1))
    else
        echo "FAIL  $verb is named outside $OWNER:"
        printf '      %s\n' $hits
        FAIL=$((FAIL + 1))
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
