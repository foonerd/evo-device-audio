#!/usr/bin/env bash
# alignment-gate-reads-stockings.test.sh — the catalogue-schemas
# alignment gate reads every `[[stockings]]` shelf the way it
# reads `[target]`.
#
# A plugin that stocks two shelves declares them as
# `[[stockings]]` blocks and has no `[target]` block. Before this
# test the gate read `[target]` only, so a stockings-only
# manifest fell out of the gate and a shelf it stocked could
# ship with no schema file. The gate now emits every (shelf,
# shape) pair from either form.
#
# Three facts, each against a throwaway repository the test
# builds:
#
#   1. A stockings-only manifest stocking `audio.delivery` v2
#      and `audio.zones` v1 is refused while `zones.v1.toml` is
#      absent, and the refusal names that file.
#   2. The same manifest passes once `zones.v1.toml` exists.
#   3. A `[target]`-only manifest is still read: its shelf with
#      no schema file is refused.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../../.." && pwd)"
GATE="$REPO_DIR/scripts/preflight/check-catalogue-schemas-alignment.sh"

PASS=0
FAIL=0
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

FAKE_REPO="$WORK/repo"
FAKE_SCHEMAS="$WORK/schemas"
mkdir -p "$FAKE_REPO/plugins/two-shelves" "$FAKE_REPO/plugins/one-shelf" \
    "$FAKE_SCHEMAS/schemas/org.evoframework/audio"

cat > "$FAKE_REPO/plugins/two-shelves/manifest.toml" <<'EOF'
[plugin]
name = "two-shelves"

[[stockings]]
shelf = "audio.delivery"
shape = 2
role  = "respondent"
request_types = ["delivery.list_outputs"]

[[stockings]]
shelf = "audio.zones"
shape = 1
role  = "respondent"
request_types = ["zone.list"]

[kind]
instance = "singleton"
EOF

cat > "$FAKE_REPO/plugins/one-shelf/manifest.toml" <<'EOF'
[plugin]
name = "one-shelf"

[target]
shelf = "audio.nowhere"
shape = 1

[kind]
instance = "singleton"
EOF

: > "$FAKE_SCHEMAS/schemas/org.evoframework/audio/delivery.v2.toml"

# The gate lists manifests through git, so the throwaway repo
# must be one.
( cd "$FAKE_REPO" && git init -q && git add -A && \
    git -c user.name=t -c user.email=t@t commit -q -m seed )

run_gate() {
    ( cd "$FAKE_REPO" && REPO_ROOT="$FAKE_REPO" \
        EVO_CATALOGUE_SCHEMAS_DIR="$FAKE_SCHEMAS" bash "$GATE" 2>&1 ) || true
}

out="$(run_gate)"
if grep -q "MISSING_SCHEMA" <<<"$out" && grep -q "audio.zones" <<<"$out" \
    && grep -q "zones.v1.toml" <<<"$out"; then
    echo "PASS  a stocked audio.zones with no zones.v1.toml is refused, naming the file"
    PASS=$((PASS + 1))
else
    echo "FAIL  a stockings-only manifest fell out of the gate:"
    printf '      %s\n' "$out"
    FAIL=$((FAIL + 1))
fi
if grep -q "audio.nowhere" <<<"$out"; then
    echo "PASS  a [target] shelf with no schema is still refused"
    PASS=$((PASS + 1))
else
    echo "FAIL  the [target] form is no longer read"
    FAIL=$((FAIL + 1))
fi
if ! grep -q "audio.delivery" <<<"$out"; then
    echo "PASS  a stocked shelf with its schema present is not named"
    PASS=$((PASS + 1))
else
    echo "FAIL  audio.delivery v2 has its schema and was still refused"
    FAIL=$((FAIL + 1))
fi

: > "$FAKE_SCHEMAS/schemas/org.evoframework/audio/zones.v1.toml"
: > "$FAKE_SCHEMAS/schemas/org.evoframework/audio/nowhere.v1.toml"
out="$(run_gate)"
if grep -q "alignment.sh: OK" <<<"$out"; then
    echo "PASS  every stocked shelf with a schema file passes"
    PASS=$((PASS + 1))
else
    echo "FAIL  the gate did not pass with every schema present:"
    printf '      %s\n' "$out"
    FAIL=$((FAIL + 1))
fi

echo "alignment-gate-reads-stockings: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
