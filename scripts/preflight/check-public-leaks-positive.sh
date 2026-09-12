#!/usr/bin/env bash
#
# check-public-leaks-positive.sh — prove the public-leak gate fires.
#
# A clean run of check-public-leaks.sh is not evidence. This
# fixture plants every class the gate defines — the list is read
# out of the gate itself, so a class added without a plant fails
# here rather than passing unproven. The plant lives in a
# throwaway git repo so the working tree stays untouched.
#
# Exits 0 only when every planted class is caught and the
# unplanted tree is clean.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GATE="${REPO_ROOT}/scripts/preflight/check-public-leaks.sh"
if [[ ! -x "${GATE}" ]]; then
    echo "positive-control: gate missing: ${GATE}" >&2
    exit 1
fi

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/leak-positive.XXXXXX")"
cleanup() { rm -rf "${WORKDIR}"; }
trap cleanup EXIT

git -C "${WORKDIR}" init -q
git -C "${WORKDIR}" config user.email "leak-positive@example.test"
git -C "${WORKDIR}" config user.name "leak-positive"
mkdir -p "${WORKDIR}/scripts/preflight"
cp "${GATE}" "${WORKDIR}/scripts/preflight/check-public-leaks.sh"
git -C "${WORKDIR}" add scripts/preflight/check-public-leaks.sh
git -C "${WORKDIR}" commit -qm "seed"

# The class list is read out of the gate itself rather than restated
# here. A class added to the gate without a plant below therefore
# fails this control instead of passing unproven — which is the whole
# point: a clean gate run may only be read as evidence for classes
# that have been shown able to fire.
mapfile -t CLASSES < <(
    grep -oE '^scan_pattern "[^"]+"' "${GATE}" \
        | sed 's/^scan_pattern "//; s/"$//'
)
if [[ ${#CLASSES[@]} -eq 0 ]]; then
    echo "positive-control: no classes found in ${GATE}." >&2
    exit 1
fi

# One trigger per class, in a path the old allow-list never scanned.
cat > "${WORKDIR}/NOTES.md" <<'EOF'
decision record ADR-0161
document SESSION_LOG and RISKS and GAPS
release narrative closure-debt
buildout Phase 2.1
parked decision PD-017
risk register R-042
scope narrative first cut
lab host 192.168.30.24
hostname pi5target
service evoproto@box
mail andser@example.test
wireless M(edia) Spot and evo-d674
EOF
git -C "${WORKDIR}" add NOTES.md
git -C "${WORKDIR}" commit -qm "plant"

if bash "${WORKDIR}/scripts/preflight/check-public-leaks.sh" \
    >"${WORKDIR}/planted.out" 2>&1; then
    echo "positive-control: planted tree was CLEAN — gate is blind." >&2
    cat "${WORKDIR}/planted.out" >&2
    exit 1
fi

# Assert on the section header the gate prints for each class, not
# on the planted literal. A literal can appear in the output because
# some other class matched the same line; only the header proves that
# this class's own pattern fired.
planted="$(cat "${WORKDIR}/planted.out")"
for class in "${CLASSES[@]}"; do
    if ! printf '%s\n' "${planted}" | grep -Fq "=== ${class} ==="; then
        echo "positive-control: class not proved: ${class}" >&2
        echo "positive-control: the gate defines it but nothing in" >&2
        echo "the plant triggers it — add a trigger to NOTES.md." >&2
        printf '%s\n' "${planted}" >&2
        exit 1
    fi
done

git -C "${WORKDIR}" rm -q NOTES.md
git -C "${WORKDIR}" commit -qm "unplant"
if ! bash "${WORKDIR}/scripts/preflight/check-public-leaks.sh" \
    >"${WORKDIR}/clean.out" 2>&1; then
    echo "positive-control: unplanted tree still FAIL." >&2
    cat "${WORKDIR}/clean.out" >&2
    exit 1
fi

echo "positive-control: ${#CLASSES[@]} classes caught; unplanted tree clean."
exit 0
