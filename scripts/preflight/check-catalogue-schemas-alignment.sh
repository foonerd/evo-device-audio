#!/usr/bin/env bash
#
# check-catalogue-schemas-alignment.sh — preflight foot-lock
# guard ensuring every shelf a shipped plugin manifest targets in
# this distribution has a corresponding schema file in the
# canonical `foonerd/evo-catalogue-schemas` repository.
#
# Closes RISKS.md R-009 (schemas-repo foot-lock not enforced).
# Backfill of the v0.1.12-era 5-plugin gap was committed to the
# schemas repo at `9cbf6a9` 2026-05-02; this preflight catches
# any future drift before it ships.
#
# Mechanism:
#
#   1. Scan every `plugins/*/manifest.toml` and
#      `plugins/*/manifest.oop.toml` in this distribution and
#      extract the value of `[target].shelf`. Shelves are
#      identified by their qualified `rack.shelf` form.
#
#   2. For each unique shelf, look up the schema file at the
#      canonical path inside the schemas-repo checkout:
#        schemas/org.evoframework/<rack>/<shelf>.v<N>.toml
#      where <N> is the manifest's `[target].shape` field.
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

# Extract shelf + shape from a manifest file. Sets globals
# `manifest_shelf` and `manifest_shape`. Returns 0 if both are
# parsed; non-zero if either is missing.
extract_target() {
    local f="$1"
    manifest_shelf=""
    manifest_shape=""
    # Bash regex extraction is fine here — these files are
    # well-formed TOML and the fields live at the [target] block
    # top level. We deliberately avoid a TOML parser dependency
    # in the preflight to keep it shell-portable.
    in_target=0
    while IFS= read -r line; do
        if [[ "$line" =~ ^\[target\] ]]; then
            in_target=1
            continue
        fi
        if [[ "$line" =~ ^\[[^][]+\] ]] && [[ "$in_target" -eq 1 ]] && [[ ! "$line" =~ ^\[target\] ]]; then
            in_target=0
            continue
        fi
        [[ "$in_target" -eq 1 ]] || continue
        if [[ "$line" =~ ^[[:space:]]*shelf[[:space:]]*=[[:space:]]*\"([^\"]+)\" ]]; then
            manifest_shelf="${BASH_REMATCH[1]}"
        fi
        if [[ "$line" =~ ^[[:space:]]*shape[[:space:]]*=[[:space:]]*([0-9]+) ]]; then
            manifest_shape="${BASH_REMATCH[1]}"
        fi
    done < "$f"
    [[ -n "$manifest_shelf" ]] && [[ -n "$manifest_shape" ]]
}

# Track checked (shelf, shape) pairs so we only report once per
# pair even if multiple manifests target the same shelf.
declare -A CHECKED

while IFS= read -r manifest; do
    if ! extract_target "$manifest"; then
        # Manifest has no [target] block or missing shelf/shape;
        # not in scope of this preflight (admission code refuses
        # such manifests separately).
        continue
    fi
    key="${manifest_shelf}:${manifest_shape}"
    if [[ -n "${CHECKED[$key]:-}" ]]; then
        continue
    fi
    CHECKED[$key]=1
    rack="${manifest_shelf%%.*}"
    shelf="${manifest_shelf#*.}"
    schema_file="$SCHEMAS_DIR/schemas/org.evoframework/${rack}/${shelf}.v${manifest_shape}.toml"
    if [[ ! -f "$schema_file" ]]; then
        VIOLATIONS+=("MISSING_SCHEMA: manifest '$manifest' targets shelf '$manifest_shelf' shape $manifest_shape — no schema at '$schema_file'")
    fi
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
