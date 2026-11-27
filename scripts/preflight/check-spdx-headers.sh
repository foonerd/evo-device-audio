#!/usr/bin/env bash
#
# check-spdx-headers.sh — preflight guard for the evo-device-audio
# IP posture. Refuses any committed Rust source file lacking an
# SPDX-License-Identifier on line 2. Every crate + plugin in this
# repo inherits the workspace-declared `Apache-2.0` license, so
# every source file must carry the matching SPDX identifier.
#
# The identifier is read as the whole expression on line 2, not its first
# word. Exactly two expressions are accepted:
#   Apache-2.0             any file
#   Apache-2.0 AND MIT     only under crates/evo-airplay/, whose files are
#                          derived from an MIT-licensed upstream and carry
#                          both licences
# Anything else (MIT alone, an OR expression, another licence, or a header
# whose line 2 is not the identifier) is a violation.
#
# Scope (a file directly under src/ or tests/ is in scope, as is one at any
# depth below them):
#   crates/*/src/**/*.rs, crates/*/tests/**/*.rs, crates/*/benches/**/*.rs, crates/*/examples/**/*.rs
#   plugins/*/src/**/*.rs, plugins/*/tests/**/*.rs, plugins/*/benches/**/*.rs, plugins/*/examples/**/*.rs
#
# Excludes generated files under target/.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$REPO_ROOT"

readonly ALLOWED_LICENSE="Apache-2.0"
# Files derived from an MIT upstream carry both licences. Only this tree.
readonly DERIVED_LICENSE="Apache-2.0 AND MIT"
readonly DERIVED_TREE="crates/evo-airplay/"

VIOLATIONS=()

collect_sources() {
    # `:(glob)` makes `**/` match zero or more directories, so a file
    # directly under src/ is listed as well as one in a subdirectory. A
    # plain `src/**/*.rs` pathspec lists only the latter.
    git ls-files \
        ':(glob)crates/*/src/**/*.rs' \
        ':(glob)crates/*/tests/**/*.rs' \
        ':(glob)crates/*/benches/**/*.rs' \
        ':(glob)crates/*/examples/**/*.rs' \
        ':(glob)plugins/*/src/**/*.rs' \
        ':(glob)plugins/*/tests/**/*.rs' \
        ':(glob)plugins/*/benches/**/*.rs' \
        ':(glob)plugins/*/examples/**/*.rs' 2>/dev/null || true
}

while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    [[ -f "$f" ]] || continue
    # The whole expression on line 2, without the comment marker or any
    # trailing space or comment close.
    spdx=$(sed -n '2p' "$f" | sed -n 's/.*SPDX-License-Identifier:[[:space:]]*//p' | sed 's/[[:space:]]*\(\*\/\)\{0,1\}[[:space:]]*$//' || true)
    if [[ -z "$spdx" ]]; then
        VIOLATIONS+=("MISSING_SPDX: $f")
        continue
    fi
    if [[ "$spdx" == "$ALLOWED_LICENSE" ]]; then
        continue
    fi
    if [[ "$spdx" == "$DERIVED_LICENSE" && "$f" == "$DERIVED_TREE"* ]]; then
        continue
    fi
    if [[ "$spdx" == "$DERIVED_LICENSE" ]]; then
        VIOLATIONS+=("UNKNOWN_LICENSE: $f carries SPDX '$spdx', allowed only under $DERIVED_TREE")
    else
        VIOLATIONS+=("UNKNOWN_LICENSE: $f carries SPDX '$spdx' (allowed: $ALLOWED_LICENSE)")
    fi
done < <(collect_sources)

if [[ ${#VIOLATIONS[@]} -eq 0 ]]; then
    echo "check-spdx-headers.sh: OK (no SPDX-header violations across crates/ + plugins/)"
    exit 0
fi

echo "check-spdx-headers.sh: FAIL"
echo "Punch list (first 50):"
for v in "${VIOLATIONS[@]:0:50}"; do
    echo "  - $v"
done
if [[ ${#VIOLATIONS[@]} -gt 50 ]]; then
    echo "  ... and $((${#VIOLATIONS[@]} - 50)) more"
fi
echo
echo "Remediation:"
echo "  Prepend to the first line of each file:"
echo "    // Copyright (c) 2026 Just a Nerd"
echo "    // SPDX-License-Identifier: Apache-2.0"
echo "  Then re-run this preflight."

exit 1
