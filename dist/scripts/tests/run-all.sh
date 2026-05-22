#!/usr/bin/env bash
# run-all.sh — driver for the distribution install-primitive
# regression test suite. Each *.test.sh under this directory
# is an independent test file with its own pass/fail tally;
# this driver runs them all and reports the aggregate.
#
# Invoke from anywhere:
#
#   bash dist/scripts/tests/run-all.sh
#
# Exit codes:
#   0 — every test file passed.
#   1 — at least one test file reported a failure.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

TOTAL=0
FAILED=0

for test_file in "$SCRIPT_DIR"/*.test.sh; do
    [[ -f "$test_file" ]] || continue
    TOTAL=$((TOTAL + 1))
    echo "=== $(basename "$test_file") ==="
    if bash "$test_file"; then
        echo "--- passed ---"
    else
        FAILED=$((FAILED + 1))
        echo "--- FAILED ---"
    fi
    echo ""
done

echo "summary: $TOTAL test files, $FAILED failed"
[[ $FAILED -eq 0 ]]
