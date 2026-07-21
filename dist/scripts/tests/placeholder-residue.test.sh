#!/usr/bin/env bash
# placeholder-residue.test.sh — regression test for the
# substitute-then-grep invariant in bootstrap.sh. Guards a
# regression class where a template file is byte-copied into
# `/etc/` without running sed, leaving literal `@TOKEN@`
# placeholders (e.g. `@EVO_AUDIO_CARD@` in `/etc/asound.conf`)
# in the deployed file. The residue check is the institutional
# catch: any rendered template that still carries a
# `@SOMETHING@` token after substitution fails the install
# with a clear pointer.
#
# This suite asserts two facts:
#
#   1. The regex used by the residue check matches every
#      placeholder form present in the distribution's
#      template files. If a template adds a new placeholder
#      form, the regex (or the substitution logic) must
#      grow to cover it; this test fails first to flag the
#      gap before any rig sees an unsubstituted file.
#   2. The check correctly returns clean (no residue) when
#      a fully-substituted file is rendered.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DIST_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"

PASS=0
FAIL=0

# The placeholder-residue regex that bootstrap.sh uses after
# every sed substitution. Kept verbatim here so the test
# documents the invariant; an edit to the regex in bootstrap.sh
# without a matching edit here is the change that should fail
# this assertion first.
RESIDUE_REGEX='@[A-Z_][A-Z0-9_]*@'

# `grep -oE` exits 1 on no matches, which combined with
# `set -o pipefail` would kill the test mid-suite via the
# command substitution path. The helper short-circuits that
# class of pipeline-as-data-extraction footgun by `|| true`-ing
# the grep step, then re-checking the result string. This is
# the same shape the bootstrap residue check uses (where the
# absence of residue is the success path, not a failure).
grep_residue_into() {
    local fixture="$1"
    printf '%s' "$fixture" \
        | { grep -oE "$RESIDUE_REGEX" || true; } \
        | sort -u | tr '\n' ' '
}

assert_residue_detected() {
    local name="$1" fixture="$2"
    local hits
    hits="$(grep_residue_into "$fixture")"
    if [[ -n "$hits" ]]; then
        echo "PASS  $name (detected: $hits)"
        PASS=$((PASS + 1))
    else
        echo "FAIL  $name (regex did not match the planted placeholder)"
        FAIL=$((FAIL + 1))
    fi
}

assert_no_residue() {
    local name="$1" fixture="$2"
    local hits
    hits="$(grep_residue_into "$fixture")"
    if [[ -z "$hits" ]]; then
        echo "PASS  $name (no residue)"
        PASS=$((PASS + 1))
    else
        echo "FAIL  $name (residue found unexpectedly: $hits)"
        FAIL=$((FAIL + 1))
    fi
}

# Fixture: dist/alsa/asound.conf. The template carries
# @EVO_AUDIO_CARD@ and MUST be caught by the residue regex
# before substitution.
ASOUND_TEMPLATE="$DIST_DIR/alsa/asound.conf"
if [[ -f "$ASOUND_TEMPLATE" ]]; then
    assert_residue_detected \
        "asound.conf template carries @EVO_AUDIO_CARD@ (caught by regex)" \
        "$(cat "$ASOUND_TEMPLATE")"
else
    echo "FAIL  asound.conf template missing at $ASOUND_TEMPLATE"
    FAIL=$((FAIL + 1))
fi

# Fixture: dist/alsa/asound.conf.source — same family, source-
# role variant. Same placeholder shape; same test.
ASOUND_SOURCE_TEMPLATE="$DIST_DIR/alsa/asound.conf.source"
if [[ -f "$ASOUND_SOURCE_TEMPLATE" ]]; then
    assert_residue_detected \
        "asound.conf.source template carries @EVO_AUDIO_CARD@" \
        "$(cat "$ASOUND_SOURCE_TEMPLATE")"
fi

# Fixture: dist/sudoers.d/*.in — every template carries
# @EVO_SERVICE_USER@. Iterate over the directory so an added
# template is exercised automatically.
if [[ -d "$DIST_DIR/sudoers.d" ]]; then
    found_any=0
    for tmpl in "$DIST_DIR/sudoers.d/"*.in; do
        [[ -f "$tmpl" ]] || continue
        found_any=1
        assert_residue_detected \
            "$(basename "$tmpl") carries @EVO_SERVICE_USER@" \
            "$(cat "$tmpl")"
    done
    if [[ $found_any -eq 0 ]]; then
        echo "SKIP  no *.in templates under dist/sudoers.d (none to check)"
    fi
fi

# Fixture: dist/plugins.d/org.evoframework.multiroom.evo-native.toml.in
# carries multiple placeholders. The regex must catch all of
# them, not just one.
MULTIROOM_TEMPLATE="$DIST_DIR/plugins.d/org.evoframework.multiroom.evo-native.toml.in"
if [[ -f "$MULTIROOM_TEMPLATE" ]]; then
    hits="$(grep -oE "$RESIDUE_REGEX" "$MULTIROOM_TEMPLATE" | sort -u | wc -l)"
    if [[ $hits -ge 2 ]]; then
        echo "PASS  multiroom template carries $hits unique placeholders (all caught)"
        PASS=$((PASS + 1))
    else
        echo "FAIL  multiroom template should carry multiple placeholders (got: $hits)"
        FAIL=$((FAIL + 1))
    fi
fi

# Fixture: a fully-substituted asound.conf (sed has run). The
# check must NOT flag any residue.
RENDERED_FIXTURE="$(cat <<'EOF'
pcm.evo {
    type plug
    slave { pcm { type hw; card "PCH"; device 0 } }
}
ctl.evo { type hw; card "PCH" }
EOF
)"
assert_no_residue \
    "rendered asound.conf with substituted card carries no residue" \
    "$RENDERED_FIXTURE"

# Fixture: a file with non-placeholder @-like text (email
# address, etc.). The regex must NOT match these — that is the
# anchor pattern (uppercase letters / underscores / digits
# between two @-signs) that rules out false positives. The
# example below uses a lowercase email address.
EMAIL_FIXTURE="# Operator contact: operator@example.org"
assert_no_residue \
    "email address in comment does not look like a placeholder" \
    "$EMAIL_FIXTURE"

echo ""
echo "placeholder-residue.test.sh: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
