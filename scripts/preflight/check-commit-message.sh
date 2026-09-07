#!/usr/bin/env bash
#
# check-commit-message.sh — scan the next commit message only.
#
# Does not walk history. The 21 public messages that already
# carry rig identity stay; rewriting them is a force-push and
# is not this gate. Call as a commit-msg hook (first argument
# is the message file) or from pre-tag-check against HEAD.
#
# Usage:
#   check-commit-message.sh [MESSAGE_FILE]
#   check-commit-message.sh --head

set -euo pipefail

if [[ "${1:-}" == "--head" ]]; then
    BODY="$(git log -1 --format=%B)"
elif [[ -n "${1:-}" ]]; then
    BODY="$(cat "$1")"
else
    echo "check-commit-message: pass a message file or --head" >&2
    exit 2
fi

PATTERN='192\.168\.30\.[0-9]{1,3}|pi5target|x64proto|nucproto|evoproto@|andser@|andrew@dt-ltd|SESSION_LOG [0-9]{4}-|ADR-[0-9]{3,}|first cut|swap later|deferred to later|follow-on release|later release|future release'

# DCO / Signed-off-by trailers already carry the maintainer
# address on every historical commit. That is identity in the
# trailer, not a leak in the message prose. Strip them so this
# gate can scan HEAD without failing the 21 we left in place.
BODY="$(printf '%s\n' "${BODY}" | grep -vE '^Signed-off-by:' || true)"

HITS="$(printf '%s\n' "${BODY}" | grep -nE "${PATTERN}" || true)"
if [[ -n "${HITS}" ]]; then
    echo "COMMIT MESSAGE LEAK."
    echo
    echo "This message carries rig identity or journal voice."
    echo "Rewrite the subject/body; do not name lab hosts, IPs,"
    echo "service users, or decision-record identifiers."
    echo
    printf '%s\n' "${HITS}"
    exit 1
fi

echo "commit-message check: clean."
exit 0
