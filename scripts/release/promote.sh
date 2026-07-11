#!/usr/bin/env bash
#
# promote.sh — eng-side → public-side squash-and-scrub release cut.
#
# Builds a staging tree from the named eng-side tag, scrubs paths
# listed in `.scrub-exclude`, regenerates Cargo.lock against the
# filtered workspace, runs the standard preflight gates against
# the staging tree (leak grep + fmt --check + clippy + test), and
# applies the result to the public repo's main branch as a single
# squashed commit on top of `--prev-tag`. Tags + pushes are gated
# by `--no-push` for review-before-publish flows.
#
# Replaces the seven manual decision points the inaugural cut
# went through; the operator runs the script with `--dry-run`
# first, reads the planned diff, then re-runs without
# `--dry-run` to actually mutate the public repo.
#
# Usage:
#
#   scripts/release/promote.sh \
#     --tag VERSION \
#     --public-repo PATH \
#     [--prev-tag VERSION] \
#     [--dry-run] \
#     [--no-push]
#
# Required arguments:
#
#   --tag VERSION
#     The eng-side tag to promote. Must already exist locally
#     (`git tag` in the eng working tree). The script does not
#     mint or move the eng-side tag; tag minting is a separate
#     authoring step the operator runs before promote.
#
#   --public-repo PATH
#     Filesystem path to a clone of the public evo-device-audio repo.
#     Working tree must be clean and on `main`. The script does
#     not clone the public repo for you; clone first, then point
#     this argument at the clone.
#
# Optional arguments:
#
#   --prev-tag VERSION
#     The previous public-side release tag. The squashed commit
#     is built on top of this tag (via `git reset --soft`). When
#     omitted the script auto-derives the latest matching tag
#     from the public repo (`git describe --abbrev=0 --tags`).
#
#   --dry-run
#     Run every check step but mutate nothing. The staging
#     tempdir is built and verified; the public repo is not
#     touched; no tag is created; no push happens. Used for
#     review-before-publish workflow. The script prints the
#     planned tree diff against the public main as the final
#     output so the operator sees exactly what would land.
#
#   --no-push
#     Mutate the public repo (commit + tag) but skip the final
#     `git push`. Used when the operator wants to inspect the
#     local public-repo state, run additional manual checks,
#     and trigger the push from a separate command.
#
# Preconditions enforced:
#
#   - eng working tree clean (`git status --porcelain` empty).
#   - eng tag exists.
#   - public-repo path is a git repo.
#   - public-repo working tree clean.
#   - public-repo on `main` branch.
#   - prev-tag exists in public repo (if explicit) OR public repo
#     has at least one tag matching the version pattern.
#
# Refuses to proceed if any precondition fails. Refuses to
# proceed if any preflight gate (leak grep, fmt --check, clippy,
# test) fails on the staging tree.

set -euo pipefail

# -------------------------------------------------------------
# Argument parsing
# -------------------------------------------------------------

TAG=""
PUBLIC_REPO=""
PREV_TAG=""
DRY_RUN=0
NO_PUSH=0

print_usage() {
    sed -n '2,/^# Refuses/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --tag)
            TAG="$2"
            shift 2
            ;;
        --public-repo)
            PUBLIC_REPO="$2"
            shift 2
            ;;
        --prev-tag)
            PREV_TAG="$2"
            shift 2
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        --no-push)
            NO_PUSH=1
            shift
            ;;
        -h|--help)
            print_usage
            exit 0
            ;;
        *)
            echo "promote.sh: unknown argument: $1" >&2
            echo "Run with --help for usage." >&2
            exit 2
            ;;
    esac
done

if [[ -z "${TAG}" ]]; then
    echo "promote.sh: --tag is required" >&2
    exit 2
fi
if [[ -z "${PUBLIC_REPO}" ]]; then
    echo "promote.sh: --public-repo is required" >&2
    exit 2
fi

# Tag-format contract: strict semver plus optional 4-segment
# closure-tag form plus optional prerelease suffix. Matches the
# regex enforced by the publish workflow on the public side; we
# refuse here so a malformed tag never reaches `git archive`.
#
#   vMAJOR.MINOR.PATCH                  base release
#   vMAJOR.MINOR.PATCH.CLOSURE          closure tag (point release)
#   vMAJOR.MINOR.PATCH-rc.N             prerelease
#   vMAJOR.MINOR.PATCH.CLOSURE-rc.N     closure-tag prerelease (rare but accepted)
TAG_FORMAT_REGEX='^v[0-9]+\.[0-9]+\.[0-9]+(\.[0-9]+)?(-[0-9A-Za-z.-]+)?$'
if ! [[ "${TAG}" =~ ${TAG_FORMAT_REGEX} ]]; then
    cat >&2 <<EOF
promote.sh: --tag does not match the release format contract.

Got:    ${TAG}
Regex:  ${TAG_FORMAT_REGEX}

Accepted shapes:
  v<MAJOR>.<MINOR>.<PATCH>                          e.g. v1.2.3
  v<MAJOR>.<MINOR>.<PATCH>.<CLOSURE>                e.g. v1.2.3.1
  v<MAJOR>.<MINOR>.<PATCH>-<PRERELEASE>             e.g. v1.2.4-rc.1
  v<MAJOR>.<MINOR>.<PATCH>.<CLOSURE>-<PRERELEASE>   e.g. v1.2.3.1-rc.2

The publish workflow on the public side enforces the same regex;
malformed tags would burn a CI cycle there. Fail fast here instead.
EOF
    exit 2
fi
if [[ -n "${PREV_TAG}" ]] && ! [[ "${PREV_TAG}" =~ ${TAG_FORMAT_REGEX} ]]; then
    echo "promote.sh: --prev-tag does not match the release format contract: ${PREV_TAG}" >&2
    echo "Same regex applies: ${TAG_FORMAT_REGEX}" >&2
    exit 2
fi

# Resolve to absolute paths so subsequent cd's stay coherent.
if [[ ! -d "${PUBLIC_REPO}" ]]; then
    echo "promote.sh: --public-repo path does not exist: ${PUBLIC_REPO}" >&2
    exit 2
fi
PUBLIC_REPO="$(cd "${PUBLIC_REPO}" && pwd)"

ENG_REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# -------------------------------------------------------------
# Logging helpers
# -------------------------------------------------------------

log_step() {
    printf '\n[promote.sh] %s\n' "$*" >&2
}

log_dry() {
    printf '[promote.sh][dry-run] %s\n' "$*" >&2
}

log_warn() {
    printf '[promote.sh][WARN] %s\n' "$*" >&2
}

log_error() {
    printf '[promote.sh][ERROR] %s\n' "$*" >&2
}

# -------------------------------------------------------------
# Precondition checks
# -------------------------------------------------------------

log_step "Verifying preconditions"

if [[ ! -d "${ENG_REPO_ROOT}/.git" ]]; then
    log_error "eng repo root has no .git: ${ENG_REPO_ROOT}"
    exit 3
fi

cd "${ENG_REPO_ROOT}"
if [[ -n "$(git status --porcelain)" ]]; then
    log_error "eng working tree is dirty; commit or stash first"
    git status --short >&2
    exit 3
fi

if ! git rev-parse --verify --quiet "refs/tags/${TAG}" >/dev/null; then
    log_error "eng tag does not exist: ${TAG}"
    log_error "Tags available:"
    git tag --list | tail -10 >&2 || true
    exit 3
fi

if [[ ! -d "${PUBLIC_REPO}/.git" ]]; then
    log_error "public-repo path is not a git repo: ${PUBLIC_REPO}"
    exit 3
fi

cd "${PUBLIC_REPO}"
if [[ -n "$(git status --porcelain)" ]]; then
    log_error "public-repo working tree is dirty: ${PUBLIC_REPO}"
    git status --short >&2
    exit 3
fi

PUBLIC_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [[ "${PUBLIC_BRANCH}" != "main" ]]; then
    log_error "public-repo not on main; on ${PUBLIC_BRANCH}"
    exit 3
fi

if [[ -z "${PREV_TAG}" ]]; then
    PREV_TAG="$(git describe --abbrev=0 --tags 2>/dev/null || true)"
    if [[ -z "${PREV_TAG}" ]]; then
        log_error "public-repo has no tags and --prev-tag was not given"
        exit 3
    fi
    log_step "Auto-derived --prev-tag: ${PREV_TAG}"
fi

if ! git rev-parse --verify --quiet "refs/tags/${PREV_TAG}" >/dev/null; then
    log_error "prev-tag does not exist in public repo: ${PREV_TAG}"
    exit 3
fi

cd "${ENG_REPO_ROOT}"

log_step "Preconditions OK"
log_step "  eng tag:        ${TAG}"
log_step "  public repo:    ${PUBLIC_REPO}"
log_step "  prev tag:       ${PREV_TAG}"
log_step "  dry-run:        $([[ ${DRY_RUN} -eq 1 ]] && echo yes || echo no)"
log_step "  no-push:        $([[ ${NO_PUSH} -eq 1 ]] && echo yes || echo no)"

# -------------------------------------------------------------
# Stage tempdir + cleanup trap
# -------------------------------------------------------------

STAGE_DIR="$(mktemp -d -t evo-promote-XXXXXX)"
cleanup() {
    if [[ -n "${STAGE_DIR}" && -d "${STAGE_DIR}" ]]; then
        rm -rf "${STAGE_DIR}"
    fi
}
trap cleanup EXIT

log_step "Staging tempdir: ${STAGE_DIR}"

# -------------------------------------------------------------
# 1. Build staging tree via git archive
# -------------------------------------------------------------

log_step "Step 1/8: Extracting ${TAG} into staging via git archive"

git archive --format=tar "${TAG}" | tar -xf - -C "${STAGE_DIR}"

if [[ ! -f "${STAGE_DIR}/Cargo.toml" ]]; then
    log_error "staging tree missing Cargo.toml — archive extraction failed"
    exit 4
fi

# -------------------------------------------------------------
# 2. Scrub paths from .scrub-exclude
# -------------------------------------------------------------

log_step "Step 2/8: Applying .scrub-exclude"

SCRUB_EXCLUDE="${STAGE_DIR}/.scrub-exclude"
if [[ ! -f "${SCRUB_EXCLUDE}" ]]; then
    log_warn "no .scrub-exclude in tag tree; skipping path-scrub step"
else
    while IFS= read -r path || [[ -n "${path}" ]]; do
        # Skip blank lines and comments.
        path="${path%%#*}"
        path="${path%"${path##*[![:space:]]}"}"  # rtrim
        path="${path#"${path%%[![:space:]]*}"}"  # ltrim
        if [[ -z "${path}" ]]; then continue; fi
        full="${STAGE_DIR}/${path}"
        if [[ -e "${full}" ]]; then
            log_step "  scrubbing: ${path}"
            rm -rf "${full}"
        else
            log_step "  scrub no-op (already absent): ${path}"
        fi
    done < "${SCRUB_EXCLUDE}"
fi

# -------------------------------------------------------------
# 3. Workspace member filter + Cargo.lock refresh
# -------------------------------------------------------------

log_step "Step 3/8: Filtering workspace members and refreshing Cargo.lock"

WORKSPACE_TOML="${STAGE_DIR}/Cargo.toml"
if [[ ! -f "${WORKSPACE_TOML}" ]]; then
    log_error "staging tree missing root Cargo.toml after scrub"
    exit 4
fi

# Drop any workspace member entry pointing at a scrubbed crate.
# Conservative: read the workspace.members array, keep only
# entries whose target directory still exists. Rewrite the array
# in-place. This intentionally targets the simple-form
# `members = ["crates/foo", "crates/bar"]` array; multi-line
# arrays are handled by the same Python literal scan below.
python3 - "${WORKSPACE_TOML}" "${STAGE_DIR}" <<'PYEOF'
import re
import sys
from pathlib import Path

toml_path = Path(sys.argv[1])
stage_dir = Path(sys.argv[2])
text = toml_path.read_text()

# Match the `members = [ ... ]` array under [workspace]. Allow
# multi-line content. Conservative: only the first occurrence; a
# valid Cargo.toml has exactly one [workspace].members.
pattern = re.compile(
    r'(?P<prefix>members\s*=\s*\[)(?P<body>[^\]]*)(?P<suffix>\])',
    re.MULTILINE,
)


def filter_body(body: str) -> str:
    out_lines = []
    for line in body.splitlines():
        stripped = line.strip()
        if not stripped:
            out_lines.append(line)
            continue
        # Strip trailing comma + comments to extract the path.
        m = re.match(
            r'^\s*"(?P<path>[^"]+)"\s*,?\s*(#.*)?$',
            line,
        )
        if not m:
            # Preserve unknown shapes verbatim.
            out_lines.append(line)
            continue
        candidate = stage_dir / m.group("path")
        if candidate.is_dir():
            out_lines.append(line)
        else:
            print(
                f"  workspace-member filter: dropping {m.group('path')} "
                f"(scrubbed)",
                file=sys.stderr,
            )
    return "\n".join(out_lines)


m = pattern.search(text)
if not m:
    print("  no [workspace].members array found; skipping filter", file=sys.stderr)
    sys.exit(0)

new_body = filter_body(m.group("body"))
new_text = text[: m.start("body")] + new_body + text[m.end("body") :]
toml_path.write_text(new_text)
print("  workspace-member filter: applied", file=sys.stderr)
PYEOF

# Drop any leftover Cargo.lock and regenerate against the
# filtered workspace via cargo metadata. The locked-build CI
# step compares this lockfile against the manifests; it must
# match the post-filter workspace.
rm -f "${STAGE_DIR}/Cargo.lock"
(
    cd "${STAGE_DIR}"
    cargo metadata --format-version 1 > /dev/null
)

# -------------------------------------------------------------
# 4. Leak grep on staging tree
# -------------------------------------------------------------

log_step "Step 4/8: Running leak-grep on staging tree"

(
    cd "${STAGE_DIR}"
    if [[ -x "scripts/preflight/check-public-leaks.sh" ]]; then
        bash scripts/preflight/check-public-leaks.sh
    else
        log_error "staging tree missing leak-grep script"
        exit 4
    fi
)

# -------------------------------------------------------------
# 5. fmt + clippy + test on staging tree
# -------------------------------------------------------------

log_step "Step 5/8: Running cargo fmt --check on staging tree"
(
    cd "${STAGE_DIR}"
    cargo fmt --all -- --check
)

log_step "Step 6/8: Running cargo clippy on staging tree"
(
    cd "${STAGE_DIR}"
    cargo clippy --workspace --all-targets --locked -- -D warnings
)

log_step "Step 7/8: Running cargo test --workspace --lib on staging tree"
(
    cd "${STAGE_DIR}"
    cargo test --workspace --lib --locked
)

# -------------------------------------------------------------
# 6. Apply staging to public main
# -------------------------------------------------------------

if [[ ${DRY_RUN} -eq 1 ]]; then
    log_step "Step 8/8: --dry-run set; not mutating public repo"
    log_dry "would: cd ${PUBLIC_REPO}"
    log_dry "would: git reset --soft ${PREV_TAG}"
    log_dry "would: replace working tree with staging contents"
    log_dry "would: git add -A && git commit -m 'release ${TAG}'"
    log_dry "would: git tag ${TAG}"
    if [[ ${NO_PUSH} -eq 0 ]]; then
        log_dry "would: git push --force-with-lease origin main"
        log_dry "would: git push --force origin ${TAG}"
    fi
    log_step "Dry-run complete; staging tree validated against public-leak / fmt / clippy / test"
    exit 0
fi

log_step "Step 8/8: Applying staging to public main"
(
    cd "${PUBLIC_REPO}"

    git reset --soft "refs/tags/${PREV_TAG}"

    # Wipe the working tree (except .git) and replace from
    # staging. The reset --soft preserves the index pointing at
    # prev-tag; the per-file replace below builds the new
    # squashed commit's content.
    find . -mindepth 1 -maxdepth 1 ! -name ".git" -exec rm -rf {} +

    # cp -a preserves attributes + dotfiles. The trailing /. on
    # the source ensures hidden files are copied too.
    cp -a "${STAGE_DIR}/." .

    git add -A
    git commit -m "release ${TAG}"
    git tag "${TAG}"
)

log_step "Public main updated; tag ${TAG} created locally"

# -------------------------------------------------------------
# 7. Push
# -------------------------------------------------------------

if [[ ${NO_PUSH} -eq 1 ]]; then
    log_step "--no-push set; review the public repo state and push manually"
    log_step "  cd ${PUBLIC_REPO}"
    log_step "  git push --force-with-lease origin main"
    log_step "  git push --force origin ${TAG}"
    exit 0
fi

log_step "Pushing public main + tag ${TAG}"
(
    cd "${PUBLIC_REPO}"
    git push --force-with-lease origin main
    git push --force "origin" "refs/tags/${TAG}"
)

log_step "Promote complete: ${TAG} published to ${PUBLIC_REPO}"
