#!/usr/bin/env bash
#
# check-public-leaks.sh — fail-fast guard against journal-voice and
# rig-identity leaks in evo-device-audio.
#
# This repository is PUBLIC. Everything committed here is published,
# including paths no release script copies, so the gate scans every
# committed path rather than an allow-list of directories. An
# allow-list is why rig hostnames and a service-user string reached
# published history unflagged: the files carrying them were simply
# outside the scanned set.
#
# Two classes, one gate:
#
#   Journal voice — decision-record identifiers, engineering-side
#   document names, release-prep narrative. Public source describes
#   what the system does, not which internal document decided it.
#
#   Rig identity — the validation fleet's addresses, hostnames,
#   service user, maintainer contact, and the wireless network names
#   it joins and broadcasts. None of it is secret, and all of it is
#   somebody's private infrastructure.
#
# Exemptions are deliberately three, and no more:
#
#   1. The guard family — this script and the two beside it — which
#      have to contain the patterns they forbid and the literals the
#      positive control plants.
#   2. The lab range is matched narrowly (192.168.30.x) so the
#      RFC1918 block 192.168.0.0/16 — legitimate technical content
#      in a LAN-trust classifier — cannot false-hit.
#   3. `evoproto` is matched only in its `user@` form, so a guard
#      that names the forbidden string in prose does not trip.
#   4. Vendored third-party trees (*/import/*, */vendor/*) are
#      exempt from the JOURNAL-VOICE patterns only. Those files are
#      upstream GPL sources we neither wrote nor may rewrite, so
#      scoring their vocabulary would be scoring somebody else's
#      prose. Rig-identity patterns still apply to them: our
#      infrastructure must not appear in any committed file,
#      including one we imported and then edited.
#
# Exits 0 when clean, 1 with a punch list otherwise.

set -eo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${REPO_ROOT}"

# The guard family: scripts whose whole purpose is to name the
# strings this gate forbids. Listed explicitly rather than by
# directory glob, so a new file under scripts/preflight/ is scanned
# like anything else and cannot inherit an exemption by living in the
# right folder.
GUARDS=(
    "scripts/preflight/check-public-leaks.sh"
    "scripts/preflight/check-public-leaks-positive.sh"
    "scripts/preflight/check-commit-message.sh"
)

# Every committed path. Binary files are skipped by grep -I.
# Exclude this script from the FILE LIST, not from grep's output.
# Filtering output by a "path:" prefix looked equivalent and was not:
# grep omits the filename when handed a single file, which happens as
# soon as xargs splits the list, and the gate then scored its own
# pattern definitions.
EXCLUDE_RE="$(printf '%s\n' "${GUARDS[@]}" \
    | sed 's/[.[\*^$]/\\&/g' | paste -sd'|' -)"
mapfile -d '' -t TRACKED < <(git ls-files -z 2>/dev/null \
    | grep -zvE "^(${EXCLUDE_RE})$" || true)
if [[ ${#TRACKED[@]} -eq 0 ]]; then
    echo "public-leak check: no committed files to scan." >&2
    exit 0
fi

FAILURES=()

# Upstream code we import verbatim. Journal-voice patterns skip it;
# rig-identity patterns do not.
VENDORED='/(import|vendor|third_party|third-party)/'

scan_pattern() {
    local label="$1" pattern="$2" scope="${3:-all}" matches
    matches=$(printf '%s\0' "${TRACKED[@]}" \
        | xargs -0 grep -HnIE "${pattern}" 2>/dev/null \
        || true)
    if [[ "${scope}" == "ours" && -n "${matches}" ]]; then
        matches=$(printf '%s\n' "${matches}" | grep -vE "${VENDORED}" || true)
    fi
    if [[ -n "${matches}" ]]; then
        FAILURES+=("=== ${label} ===" "${matches}" "")
    fi
}

# ---------------------------- journal voice ----------------------------

scan_pattern "Decision-record identifiers (rewrite descriptively)" \
    '\bADR-[0-9]{3,}\b' \
    "ours"

scan_pattern "Engineering-side document names" \
    '\b(SESSION_LOG|RISKS|PARKED_DECISIONS|V0\.[0-9]+\.[0-9]+_SCOPE|VENDOR_EXTENSION_OPTIONS)\b' \
    "ours"

scan_pattern "Release-prep narrative term 'closure-debt'" \
    'closure-debt|closure debt' \
    "ours"

scan_pattern "Buildout-phase identifiers (Phase X.Y)" \
    'Phase [0-9]+\.[A-Za-z0-9]+|Phase [A-Z]\.[0-9]+' \
    "ours"

scan_pattern "Parked-decision identifiers (PD-NNN)" \
    '\bPD-[0-9]+\b' \
    "ours"

scan_pattern "Risk-register identifiers (R-NNN)" \
    '\bR-[0-9]{3,}\b' \
    "ours"

scan_pattern "GAPS document references" \
    '\bGAPS\b' \
    "ours"

# Scope-narrowing narrative. The bare words "defer" / "deferral" are
# NOT matched here: they name real mechanisms in this source (a
# shared-PHY deferral gate, deferred descriptor cleanup), and a gate
# that forces those to be renamed would be scoring vocabulary rather
# than leaks. Planning-voice deferral is caught where it belongs, in
# the commit-message hook.
scan_pattern "Scope-narrowing narrative in source" \
    '(first cut|swap later|smallest fix|first pass|for now, |later release|future release)' \
    "ours"

# ---------------------------- rig identity -----------------------------

scan_pattern "Validation-rig IP addresses" \
    '192\.168\.30\.[0-9]{1,3}'

scan_pattern "Validation-rig hostnames" \
    '\b(pi5target|x64proto|nucproto)\b'

scan_pattern "Service-user account in user@host form" \
    'evoproto@'

scan_pattern "Maintainer contact addresses" \
    '(andser@|andrew@dt-ltd)'

# The fleet's own wireless network names: the network the rigs join
# and the hotspot name a specific unit derives from its MAC.
#
# Deliberately two literals, not a generic SSID or MAC class. A
# pattern shaped like "any SSID" or "any MAC" would fail every
# honest wifi fixture in this repo — parsers for `iw` output have to
# carry realistic sample text — and the leak is not the shape of an
# SSID, it is these two names. Illustrative fixture values built
# from the documented placeholder MAC (aa:11:22:33:44:66, whence
# `evo-4466`) are not devices and are not matched.
scan_pattern "Fleet wireless network names" \
    'M\(edia\) Spot|evo-d674'

# ------------------------------- verdict -------------------------------

if [[ ${#FAILURES[@]} -eq 0 ]]; then
    echo "public-leak check: clean (${#TRACKED[@]} committed paths scanned)."
    exit 0
fi

echo "PUBLIC-LEAK CHECK FAILED."
echo
echo "This repository is public. The lines below carry either"
echo "engineering-side narrative or validation-rig identity."
echo "Rewrite source to state the constraint or the behaviour"
echo "directly; replace rig addresses, hostnames and accounts with"
echo "a placeholder or an operator-supplied value."
echo
printf '%s\n' "${FAILURES[@]}"
echo
echo "Run again after rewriting; the gate exits 0 only when zero hits."
exit 1
