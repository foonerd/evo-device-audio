#!/usr/bin/env bash
#
# check-spdx-headers.sh — preflight guard for the evo-device-audio
# IP posture. Refuses any committed Rust source file lacking an
# SPDX-License-Identifier on line 2. Every crate + plugin in this
# repo inherits the workspace-declared `Apache-2.0` license, so
# every source file must carry the matching SPDX identifier.
#
# Scope:
#   crates/*/src/**/*.rs, crates/*/tests/**/*.rs, crates/*/benches/**/*.rs, crates/*/examples/**/*.rs
#   plugins/*/src/**/*.rs, plugins/*/tests/**/*.rs, plugins/*/benches/**/*.rs, plugins/*/examples/**/*.rs
#
# Excludes generated files under target/.

set -euo pipefail

REPO_ROOT="${REPO_ROOT:-$(git rev-parse --show-toplevel)}"
cd "$REPO_ROOT"

readonly ALLOWED_LICENSE="Apache-2.0"

VIOLATIONS=()

collect_sources() {
    git ls-files \
        'crates/*/src/**/*.rs' \
        'crates/*/tests/**/*.rs' \
        'crates/*/benches/**/*.rs' \
        'crates/*/examples/**/*.rs' \
        'plugins/*/src/**/*.rs' \
        'plugins/*/tests/**/*.rs' \
        'plugins/*/benches/**/*.rs' \
        'plugins/*/examples/**/*.rs' 2>/dev/null || true
}

while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    [[ -f "$f" ]] || continue
    spdx=$(sed -n '2p' "$f" | grep -oE 'SPDX-License-Identifier: \S+' | sed 's/^SPDX-License-Identifier: //' || true)
    if [[ -z "$spdx" ]]; then
        VIOLATIONS+=("MISSING_SPDX: $f")
        continue
    fi
    if [[ "$spdx" != "$ALLOWED_LICENSE" ]]; then
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
