#!/usr/bin/env bash
#
# check-public-leaks.sh — fail-fast guard against journal-voice leaks
# in evo-device-audio source. Same discipline as evo-core-eng's
# scanner, adapted to this repo's layout (plugins/ + crates/ + dist/
# + scripts/).
#
# Catches: ADR references in source, engineering-side document
# filenames (SESSION_LOG / RISKS / PARKED_DECISIONS / V0.x.y_SCOPE /
# VENDOR_EXTENSION_OPTIONS), closure-debt narrative term,
# buildout-phase identifiers, parked-decision identifiers,
# risk-register identifiers, GAPS references.

set -eo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

SCAN_PATHS=(
    "plugins"
    "crates"
    "dist"
    "scripts"
    "acceptance"
)

SCAN_EXTS=(
    "*.rs"
    "*.toml"
    "*.md"
    "*.sql"
    "*.sh"
    "*.json"
    "*.yaml"
    "*.yml"
)

INCLUDE_ARGS=()
for ext in "${SCAN_EXTS[@]}"; do
    INCLUDE_ARGS+=(--include="${ext}")
done

# The preflight script itself encodes the patterns it scans for;
# excluding it is the only way the script can co-exist with the gate.
EXCLUDE_ARGS=(
    "--exclude-dir=target"
    "--exclude-dir=.cargo"
    "--exclude-dir=node_modules"
    "--exclude=check-public-leaks.sh"
)

EXISTING_PATHS=()
for path in "${SCAN_PATHS[@]}"; do
    if [[ -e "${path}" ]]; then
        EXISTING_PATHS+=("${path}")
    fi
done

FAILURES=()

scan_pattern() {
    local label="$1"
    local pattern="$2"
    local matches
    matches=$(grep -rnE "${INCLUDE_ARGS[@]}" "${EXCLUDE_ARGS[@]}" "${pattern}" "${EXISTING_PATHS[@]}" 2>/dev/null || true)
    if [[ -n "${matches}" ]]; then
        FAILURES+=("=== ${label} ===")
        FAILURES+=("${matches}")
        FAILURES+=("")
    fi
}

# Pattern 1: ADR identifiers.
scan_pattern \
    "ADR identifiers in source (rewrite descriptively)" \
    '\bADR-[0-9]{3,}\b'

# Pattern 2: engineering-side document filenames.
scan_pattern \
    "Engineering-side document filenames in source" \
    '\b(SESSION_LOG|RISKS|PARKED_DECISIONS|V0\.[0-9]+\.[0-9]+_SCOPE|VENDOR_EXTENSION_OPTIONS)\b'

# Pattern 3: closure-debt narrative term.
scan_pattern \
    "Release-prep narrative term 'closure-debt' in source" \
    'closure-debt|closure debt'

# Pattern 4: buildout-phase identifiers.
scan_pattern \
    "Buildout-phase identifiers (Phase X.Y) in source" \
    'Phase [0-9]+\.[A-Za-z0-9]+|Phase [A-Z]\.[0-9]+'

# Pattern 5: parked-decision identifiers.
scan_pattern \
    "Parked-decision identifiers (PD-NNN) in source" \
    '\bPD-[0-9]+\b'

# Pattern 6: risk-register identifiers.
scan_pattern \
    "Risk-register identifiers (R-NNN) in source" \
    '\bR-[0-9]{3,}\b'

# Pattern 7: GAPS document references.
scan_pattern \
    "GAPS document references in source" \
    '\bGAPS\b'

if [[ ${#FAILURES[@]} -eq 0 ]]; then
    echo "public-leak check: clean."
    exit 0
fi

echo "PUBLIC-LEAK CHECK FAILED."
echo
echo "The patterns below appear in distribution source / config /"
echo "scripts. These trees ship to the public release repository;"
echo "rewrite the matching lines as descriptive prose (state the"
echo "constraint or the plugin's current behaviour, not which"
echo "engineering document decided it or which release first"
echo "surfaced it)."
echo
printf '%s\n' "${FAILURES[@]}"
echo
echo "Run again after rewriting; the gate exits 0 only when zero hits."
exit 1
