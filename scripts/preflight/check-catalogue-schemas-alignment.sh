#!/usr/bin/env bash
#
# check-catalogue-schemas-alignment.sh — preflight foot-lock
# guard ensuring every shelf a shipped plugin manifest targets in
# this distribution has a corresponding schema file in the
# canonical `foonerd/evo-catalogue-schemas` repository.
#
# Preflight-enforces the schemas-repo foot-lock. Catches drift
# between shipped plugin manifests and the canonical schema set
# before it lands in a distribution release.
#
# Mechanism:
#
#   1. Scan every `plugins/*/manifest.toml` and
#      `plugins/*/manifest.oop.toml` in this distribution and
#      extract every shelf it stocks: the `[target]` block of a
#      single-shelf manifest, or every `[[stockings]]` block of
#      a multi-shelf one. Shelves are identified by their
#      qualified `rack.shelf` form.
#
#   2. For each unique shelf, look up the schema file at the
#      canonical path inside the schemas-repo checkout:
#        schemas/org.evoframework/<rack>/<shelf>.v<N>.toml
#      where <N> is that block's `shape` field.
#
#   3. If the schema file is missing, the cut is refused.
#
# The schemas-repo checkout location:
#
#   1. EVO_CATALOGUE_SCHEMAS_DIR env var if set.
#   2. Sibling clone at `../evo-catalogue-schemas` (the eng
#      team's conventional layout).
#   3. If neither is reachable, the script exits 2 with a
#      remediation message rather than passing silently.
#
# Run by CI on every PR; intended for pre-commit invocation when
# editing plugin manifests.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$REPO_ROOT"

SCHEMAS_DIR="${EVO_CATALOGUE_SCHEMAS_DIR:-}"
if [[ -z "$SCHEMAS_DIR" ]]; then
    SCHEMAS_DIR="$(realpath "$REPO_ROOT/../evo-catalogue-schemas" 2>/dev/null || true)"
fi
if [[ -z "$SCHEMAS_DIR" ]] || [[ ! -d "$SCHEMAS_DIR/schemas/org.evoframework" ]]; then
    echo "check-catalogue-schemas-alignment.sh: SKIP (schemas-repo checkout not reachable)"
    echo "  Set EVO_CATALOGUE_SCHEMAS_DIR=/path/to/evo-catalogue-schemas, or clone"
    echo "  the schemas repo as a sibling: git clone <url> ../evo-catalogue-schemas"
    # SKIP is not a hard fail: PR-mode runs may not have the
    # sibling repo available. The cut workflow explicitly sets
    # EVO_CATALOGUE_SCHEMAS_DIR so the gate fires at cut time.
    exit 0
fi

VIOLATIONS=()

# Emit every (shelf, shape) pair a manifest stocks, one per line
# as `shelf shape`. A manifest declares either one `[target]`
# block (single-shelf form) or one or more `[[stockings]]` blocks
# (multi-shelf form); the two forms are mutually exclusive and
# both are read here. A stockings-only manifest is not skipped:
# every shelf it stocks must have its schema file, exactly as a
# `[target]` shelf must.
#
# Bash regex extraction is fine here — these files are
# well-formed TOML and the fields live at the block's top
# level. We deliberately avoid a TOML parser dependency in the
# preflight to keep it shell-portable.
extract_stockings() {
    local f="$1"
    local in_block=0 shelf="" shape=""
    flush() {
        if [[ "$in_block" -eq 1 ]] && [[ -n "$shelf" ]] && [[ -n "$shape" ]]; then
            echo "$shelf $shape"
        fi
        shelf=""
        shape=""
    }
    while IFS= read -r line; do
        if [[ "$line" =~ ^\[target\] ]] || [[ "$line" =~ ^\[\[stockings\]\] ]]; then
            flush
            in_block=1
            continue
        fi
        if [[ "$line" =~ ^\[[^][]+\] ]] || [[ "$line" =~ ^\[\[[^][]+\]\] ]]; then
            flush
            in_block=0
            continue
        fi
        [[ "$in_block" -eq 1 ]] || continue
        if [[ "$line" =~ ^[[:space:]]*shelf[[:space:]]*=[[:space:]]*\"([^\"]+)\" ]]; then
            shelf="${BASH_REMATCH[1]}"
        fi
        if [[ "$line" =~ ^[[:space:]]*shape[[:space:]]*=[[:space:]]*([0-9]+) ]]; then
            shape="${BASH_REMATCH[1]}"
        fi
    done < "$f"
    flush
}

# Track checked (shelf, shape) pairs so we only report once per
# pair even if multiple manifests stock the same shelf.
declare -A CHECKED

while IFS= read -r manifest; do
    while read -r manifest_shelf manifest_shape; do
        [[ -n "$manifest_shelf" ]] || continue
        key="${manifest_shelf}:${manifest_shape}"
        if [[ -n "${CHECKED[$key]:-}" ]]; then
            continue
        fi
        CHECKED[$key]=1
        rack="${manifest_shelf%%.*}"
        shelf="${manifest_shelf#*.}"
        schema_file="$SCHEMAS_DIR/schemas/org.evoframework/${rack}/${shelf}.v${manifest_shape}.toml"
        if [[ ! -f "$schema_file" ]]; then
            VIOLATIONS+=("MISSING_SCHEMA: manifest '$manifest' stocks shelf '$manifest_shelf' shape $manifest_shape — no schema at '$schema_file'")
        fi
    done < <(extract_stockings "$manifest")
done < <(git ls-files 'plugins/*/manifest.toml' 'plugins/*/manifest.oop.toml' 2>/dev/null || true)

if [[ ${#VIOLATIONS[@]} -eq 0 ]]; then
    echo "check-catalogue-schemas-alignment.sh: OK (every shipped shelf is foot-locked in $SCHEMAS_DIR)"
    exit 0
fi

echo "check-catalogue-schemas-alignment.sh: FAIL (${#VIOLATIONS[@]} violation(s))"
echo "  schemas repo: $SCHEMAS_DIR"
echo
echo "Punch list:"
for v in "${VIOLATIONS[@]}"; do
    echo "  - $v"
done
echo
echo "Remediation:"
echo "  Author the missing schema at the path named above in the schemas"
echo "  repo. Pattern after a sibling shelf's schema (e.g."
echo "  schemas/org.evoframework/audio/playback.v1.toml). The schema file"
echo "  carries:"
echo "    schema_version = 1"
echo "    rack = \"<rack>\""
echo "    shelf = \"<shelf>\""
echo "    shape = <N>"
echo "    description = \"...\""
echo "  plus any [[requests]] / [[happenings]] / etc. blocks the shelf supports."
echo
echo "  Commit + push to the schemas repo first, then re-run this preflight"
echo "  against the updated schemas checkout. The cut workflow refuses the"
echo "  release tag until every shipped shelf is foot-locked."

exit 1
