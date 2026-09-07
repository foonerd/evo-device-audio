#!/usr/bin/env bash
#
# check-public-leaks-positive.sh — prove the public-leak gate fires.
#
# A clean run of check-public-leaks.sh is not evidence. This
# fixture plants the four classes that missed the allow-list
# scanner, in a path that allow-list would have skipped (repo
# root), then asserts fail-then-clean. The plant lives in a
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

# Path the old allow-list never scanned.
cat > "${WORKDIR}/NOTES.md" <<'EOF'
lab host 192.168.30.24
hostname pi5target
service evoproto@box
mail andser@example.test
EOF
git -C "${WORKDIR}" add NOTES.md
git -C "${WORKDIR}" commit -qm "plant"

if bash "${WORKDIR}/scripts/preflight/check-public-leaks.sh" \
    >"${WORKDIR}/planted.out" 2>&1; then
    echo "positive-control: planted tree was CLEAN — gate is blind." >&2
    cat "${WORKDIR}/planted.out" >&2
    exit 1
fi

planted="$(cat "${WORKDIR}/planted.out")"
for class in \
    '192\.168\.30\.24' \
    'pi5target' \
    'evoproto@' \
    'andser@'
do
    if ! printf '%s\n' "${planted}" | grep -Eq "${class}"; then
        echo "positive-control: planted ${class} was not reported." >&2
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

echo "positive-control: four classes caught; unplanted tree clean."
exit 0
