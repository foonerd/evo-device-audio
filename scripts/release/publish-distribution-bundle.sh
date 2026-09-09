#!/usr/bin/env bash
#
# publish-distribution-bundle.sh — first-boot tarball bake.
#
# Two stores, one rule:
#   GitHub Release <tag> holds the tarball bytes (git cannot).
#   bundles/distribution/<version>.toml is a projection of that
#   Release. It is never written from compose tempdir hashes.
#
# A published Release asset is frozen. compose embeds
# built_at_utc inside the tarball, so a second compose cannot
# reproduce the published sha256. Missing triple = compose and
# append. Present triple = do not touch the bytes.
#
# The pointer writer is scripts/release/write-distribution-pointer.sh.
# It uses jq(1) on `gh release view --json assets`. It does not
# use `gh --jq --arg` (gh --jq takes one expression).
#
# Creates GitHub Release <tag> with --latest=false and does not
# touch GitHub Release v0.1.13.
#
set -euo pipefail

TAG=""
ARTEFACTS_REPO=""
BUNDLE_DIR="${EVO_BUNDLE_OUT_DIR:-}"
CARGO_VERSION=""
DRY_RUN=0
NO_PUSH=0
NO_GH_RELEASE=0
PUBLISH_ONLY=0

TRIPLES=(
    x86_64-unknown-linux-gnu
    aarch64-unknown-linux-gnu
    armv7-unknown-linux-gnueabihf
)

PUBLIC_TAG_REPOS=(
    foonerd/evo-core
    foonerd/evo-device-audio-ui
    foonerd/evo-kiosk
)

TAG_FORMAT_REGEX='^v[0-9]+\.[0-9]+\.[0-9]+(\.[0-9]+)?(-[0-9A-Za-z.-]+)?$'
PROTECTED_GH_RELEASE="v0.1.13"
ARTEFACTS_GH_REPO="foonerd/evo-device-audio-artefacts"

usage() {
    cat >&2 <<'EOF'
usage: scripts/release/publish-distribution-bundle.sh \
    --tag v0.1.13.0 \
    --artefacts-repo PATH \
    [--bundle-dir PATH] \
    [--cargo-version 0.1.13] \
    [--publish-only] \
    [--dry-run] \
    [--no-push] \
    [--no-gh-release]
EOF
    exit 2
}

log_step() { printf '\n[publish-bundle] %s\n' "$*" >&2; }
log_ok()   { printf '[publish-bundle] OK: %s\n' "$*" >&2; }
log_fail() { printf '[publish-bundle] FAIL: %s\n' "$*" >&2; }
log_dry()  { printf '[publish-bundle][dry-run] %s\n' "$*" >&2; }
die()      { log_fail "$*"; exit 3; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --artefacts-repo) ARTEFACTS_REPO="$2"; shift 2 ;;
        --tag)            TAG="$2"; shift 2 ;;
        --bundle-dir)     BUNDLE_DIR="$2"; shift 2 ;;
        --cargo-version)  CARGO_VERSION="$2"; shift 2 ;;
        --publish-only)   PUBLISH_ONLY=1; shift ;;
        --dry-run)        DRY_RUN=1; shift ;;
        --no-push)        NO_PUSH=1; shift ;;
        --no-gh-release)  NO_GH_RELEASE=1; shift ;;
        -h|--help)        usage ;;
        *) log_fail "unknown argument: $1"; usage ;;
    esac
done

[[ -n "${TAG}" ]]            || { log_fail "--tag is required"; exit 2; }
[[ -n "${ARTEFACTS_REPO}" ]] || { log_fail "--artefacts-repo is required"; exit 2; }
if ! [[ "${TAG}" =~ ${TAG_FORMAT_REGEX} ]]; then
    log_fail "--tag does not match the release format: ${TAG}"
    exit 2
fi
if [[ "${TAG}" == "${PROTECTED_GH_RELEASE}" ]]; then
    log_fail "refusing to publish over GitHub Release ${PROTECTED_GH_RELEASE}"
    exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
POINTER_WRITER="${SCRIPT_DIR}/write-distribution-pointer.sh"
[[ -x "${POINTER_WRITER}" ]] || die "missing executable ${POINTER_WRITER}"

unset CARGO_TARGET_DIR

if [[ -z "${CARGO_VERSION}" ]]; then
    CARGO_VERSION="$(awk -F'"' '/^version =/ {print $2; exit}' "${REPO_ROOT}/Cargo.toml")"
fi
[[ -n "${CARGO_VERSION}" ]] || die "could not read workspace version"

ARTEFACTS_REPO="$(cd "${ARTEFACTS_REPO}" && pwd)"
if [[ ! -d "${ARTEFACTS_REPO}/.git" && ! -f "${ARTEFACTS_REPO}/.git" ]]; then
    die "artefacts path is not a git repo: ${ARTEFACTS_REPO}"
fi
if [[ -n "$(git -C "${ARTEFACTS_REPO}" status --porcelain)" ]]; then
    die "artefacts working tree is dirty: ${ARTEFACTS_REPO}"
fi

require_public_tag() {
    local repo="$1"
    if ! git ls-remote --exit-code --tags "https://github.com/${repo}.git" "refs/tags/${TAG}" >/dev/null; then
        die "public ${repo} has no tag ${TAG}"
    fi
    log_ok "public ${repo} tag ${TAG}"
}

tarball_name() {
    printf 'evo-device-audio-%s-%s.tar.gz' "$1" "${CARGO_VERSION}"
}

asset_on_release() {
    local needle="$1"
    printf '%s' "${RELEASE_ASSETS_JSON}" | jq -e --arg n "${needle}" \
        '.assets[] | select(.name==$n)' >/dev/null
}

load_release_inventory() {
    RELEASE_ASSETS_JSON=""
    RELEASE_EXISTS=0
    if [[ "${NO_GH_RELEASE}" -eq 1 ]]; then
        return
    fi
    if gh release view "${TAG}" --repo "${ARTEFACTS_GH_REPO}" >/dev/null 2>&1; then
        RELEASE_EXISTS=1
        RELEASE_ASSETS_JSON="$(gh release view "${TAG}" --repo "${ARTEFACTS_GH_REPO}" --json assets)"
        log_ok "GitHub Release ${TAG} exists; published assets are frozen"
    fi
}

triple_is_frozen() {
    local triple="$1" base
    base="$(tarball_name "${triple}")"
    [[ "${RELEASE_EXISTS}" -eq 1 ]] || return 1
    asset_on_release "${base}" || return 1
    asset_on_release "${base}.sig" || return 1
    asset_on_release "${base}.sha256" || return 1
}

log_step "Cut ${TAG} (workspace ${CARGO_VERSION})"

for skip_var in EVO_BUNDLE_SKIP_BOOT_LAYER EVO_BUNDLE_SKIP_KIOSK_LAYER EVO_BUNDLE_SKIP_UI_SHELL; do
    if [[ "${!skip_var:-0}" != "0" ]]; then
        die "${skip_var} is set; skip-layer is a refuse"
    fi
done

log_step "Public sibling tags"
for repo in "${PUBLIC_TAG_REPOS[@]}"; do
    require_public_tag "${repo}"
done

if [[ -z "${BUNDLE_DIR}" ]]; then
    BUNDLE_DIR="$(mktemp -d -t evo-bundles-XXXXXX)"
fi
mkdir -p "${BUNDLE_DIR}"
BUNDLE_DIR="$(cd "${BUNDLE_DIR}" && pwd)"

load_release_inventory

COMPOSE_TRIPLES=()
FROZEN_TRIPLES=()
for triple in "${TRIPLES[@]}"; do
    if [[ "${PUBLISH_ONLY}" -eq 0 ]] && triple_is_frozen "${triple}"; then
        FROZEN_TRIPLES+=("${triple}")
        log_ok "frozen $(tarball_name "${triple}"); will not recompose"
    else
        COMPOSE_TRIPLES+=("${triple}")
    fi
done

if [[ "${PUBLISH_ONLY}" -eq 0 ]]; then
    [[ -n "${EVO_PLUGIN_SIGNING_KEY:-}" ]] || die "EVO_PLUGIN_SIGNING_KEY is unset"
    [[ -r "${EVO_PLUGIN_SIGNING_KEY}" ]] || die "EVO_PLUGIN_SIGNING_KEY is not readable"
    # shellcheck source=../../dist/scripts/lib/oop-plugins.sh
    source "${REPO_ROOT}/dist/scripts/lib/oop-plugins.sh"
fi

if [[ "${DRY_RUN}" -eq 1 ]]; then
    if [[ ${#COMPOSE_TRIPLES[@]} -eq 0 ]]; then
        log_dry "would: compose nothing; all triples frozen"
    else
        log_dry "would: compose ${COMPOSE_TRIPLES[*]} from piece-pins.toml"
    fi
    log_dry "would: upload only newly composed assets"
    log_dry "would: write pointer as a projection of Release ${TAG}"
    log_dry "would: leave GitHub Release ${PROTECTED_GH_RELEASE} and Latest untouched"
    log_ok "dry-run complete; artefacts repo not mutated"
    exit 0
fi

if [[ "${PUBLISH_ONLY}" -eq 0 && ${#COMPOSE_TRIPLES[@]} -gt 0 ]]; then
    log_step "Compose missing tarballs from published pieces"
    export EVO_BUNDLE_OUT_DIR="${BUNDLE_DIR}"
    for triple in "${COMPOSE_TRIPLES[@]}"; do
        bash "${SCRIPT_DIR}/compose-distribution-from-pieces.sh" \
            --target "${triple}" \
            --artefacts-repo "${ARTEFACTS_REPO}" \
            --bundle-dir "${BUNDLE_DIR}" \
            --cargo-version "${CARGO_VERSION}"
    done
elif [[ "${PUBLISH_ONLY}" -eq 0 ]]; then
    log_step "All triples frozen on Release ${TAG}"
fi

UPLOAD=()
for triple in "${COMPOSE_TRIPLES[@]+"${COMPOSE_TRIPLES[@]}"}"; do
    base="$(tarball_name "${triple}")"
    [[ -f "${BUNDLE_DIR}/${base}" ]] || die "missing ${BUNDLE_DIR}/${base}"
    [[ -f "${BUNDLE_DIR}/${base}.sig" ]] || die "missing ${BUNDLE_DIR}/${base}.sig"
    (cd "${BUNDLE_DIR}" && sha256sum "${base}" > "${base}.sha256")
    UPLOAD+=("${base}" "${base}.sig" "${base}.sha256")
    log_ok "staged ${base} ($(wc -c < "${BUNDLE_DIR}/${base}") bytes)"
done

if [[ "${NO_GH_RELEASE}" -eq 1 ]]; then
    log_ok "--no-gh-release set; not uploading; not writing pointer"
    exit 0
fi

log_step "GitHub Release ${TAG} (store; Latest untouched)"
if gh release view "${PROTECTED_GH_RELEASE}" --repo "${ARTEFACTS_GH_REPO}" >/dev/null 2>&1; then
    log_ok "GitHub Release ${PROTECTED_GH_RELEASE} present; will not modify it"
fi

if [[ "${RELEASE_EXISTS}" -eq 0 ]]; then
    if [[ ${#UPLOAD[@]} -eq 0 ]]; then
        die "Release ${TAG} does not exist and nothing was composed to create it"
    fi
    assets=()
    for f in "${UPLOAD[@]}"; do
        assets+=("${BUNDLE_DIR}/${f}")
    done
    gh release create "${TAG}" \
        --repo "${ARTEFACTS_GH_REPO}" \
        --title "evo-device-audio ${TAG}" \
        --notes "Distribution tarball ${CARGO_VERSION}. Installer version pin stays ${CARGO_VERSION}. Bytes live on this Release. GitHub Release ${PROTECTED_GH_RELEASE} and Latest are unchanged." \
        --latest=false \
        "${assets[@]}"
    log_ok "created Release ${TAG}; Latest and ${PROTECTED_GH_RELEASE} untouched"
else
    for f in "${UPLOAD[@]+"${UPLOAD[@]}"}"; do
        if asset_on_release "${f}"; then
            log_ok "keep frozen asset ${f}"
            continue
        fi
        gh release upload "${TAG}" "${BUNDLE_DIR}/${f}" --repo "${ARTEFACTS_GH_REPO}"
        log_ok "appended asset ${f}"
    done
fi

log_step "Pointer from the store (not from compose tempdir)"
POINTER_REL="bundles/distribution/${CARGO_VERSION}.toml"
POINTER_PATH="${ARTEFACTS_REPO}/${POINTER_REL}"
mkdir -p "$(dirname "${POINTER_PATH}")"
bash "${POINTER_WRITER}" \
    --tag "${TAG}" \
    --version "${CARGO_VERSION}" \
    --out "${POINTER_PATH}" \
    --repo "${ARTEFACTS_GH_REPO}"

if [[ -n "${EVO_PLUGIN_SIGNING_KEY:-}" && -r "${EVO_PLUGIN_SIGNING_KEY}" ]]; then
    openssl pkeyutl -sign \
        -inkey "${EVO_PLUGIN_SIGNING_KEY}" -rawin \
        -in "${POINTER_PATH}" \
        -out "${POINTER_PATH%.toml}.sig"
fi

log_step "Artefacts commit (pointer only)"
(
    cd "${ARTEFACTS_REPO}"
    branch="$(git rev-parse --abbrev-ref HEAD)"
    git fetch origin
    git pull --rebase "origin" "${branch}"
    git add "${POINTER_REL}"
    if [[ -f "${POINTER_PATH%.toml}.sig" ]]; then
        git add "${POINTER_REL%.toml}.sig"
    fi
    if git diff --cached --quiet; then
        log_ok "artefacts already contain ${POINTER_REL}; no new commit"
    else
        git commit --signoff -m "release ${TAG} distribution pointer"
        if [[ "${NO_PUSH}" -eq 1 ]]; then
            log_ok "--no-push set; review and push ${ARTEFACTS_REPO} by hand"
        else
            tries=0
            until git push origin HEAD; do
                tries=$((tries + 1))
                if [[ "${tries}" -ge 3 ]]; then
                    die "artefacts pointer push rejected after ${tries} attempts"
                fi
                log_step "artefacts push rejected; rebase onto origin/${branch} (${tries})"
                git fetch origin
                git pull --rebase "origin" "${branch}"
            done
            log_ok "pushed artefacts pointer"
        fi
    fi
)
