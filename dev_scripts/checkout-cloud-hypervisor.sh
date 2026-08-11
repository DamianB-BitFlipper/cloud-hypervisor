#!/usr/bin/env bash
# checkout-cloud-hypervisor.sh
#
# Clone (or update) the cloud-hypervisor repository at a specific ref.
#
# Intended to run before dev_scripts/fetch-assets.sh and
# dev_scripts/snapshot-test.sh, on a fresh aarch64 Linux host.
#
# Usage: checkout-cloud-hypervisor.sh [options]
#
# Options:
#   --dest DIR     Destination directory (default: $HOME/cloud-hypervisor)
#   --ref REF      Branch, tag, or commit to check out (default: main)
#   --depth N      Shallow clone depth (default: full clone)
#   --force        Remove an existing destination before cloning
#   -h, --help     Show this help

set -euo pipefail

REPO_URL="https://github.com/cloud-hypervisor/cloud-hypervisor.git"

DEST="${DEST:-$HOME/cloud-hypervisor}"
REF="${REF:-main}"
DEPTH=""
FORCE=0

log() { printf '[checkout-ch] %s\n' "$*"; }
err() { printf '[checkout-ch] ERROR: %s\n' "$*" >&2; }
die() { err "$*"; exit 1; }

usage() {
    sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
}

# ---- Arg parsing -------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --dest)   DEST="$2"; shift 2 ;;
        --ref)    REF="$2"; shift 2 ;;
        --depth)  DEPTH="$2"; shift 2 ;;
        --force)  FORCE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) err "unknown argument: $1"; usage >&2; exit 2 ;;
    esac
done

# ---- Ensure git is available -------------------------------------------------
if ! command -v git >/dev/null 2>&1; then
    log "git not found; installing via apt"
    sudo apt-get update
    sudo apt-get install -y git
fi

# ---- Handle an existing destination ------------------------------------------
if [[ -e "$DEST" ]]; then
    if [[ $FORCE -eq 1 ]]; then
        log "Removing existing destination (per --force): $DEST"
        rm -rf "$DEST"
    elif [[ -d "$DEST/.git" ]]; then
        log "Destination already contains a git repo: $DEST"
        log "Fetching and checking out ref: $REF"
        git -C "$DEST" fetch --tags --prune origin
        git -C "$DEST" checkout "$REF"
        # If REF is a branch, fast-forward it; ignore failure for detached/tag refs.
        git -C "$DEST" pull --ff-only origin "$REF" 2>/dev/null || true
        log "Repo at $(git -C "$DEST" rev-parse --short HEAD) ($REF)"
        exit 0
    else
        die "destination exists and is not a git repo: $DEST (use --force to overwrite)"
    fi
fi

# ---- Fresh clone -------------------------------------------------------------
mkdir -p "$(dirname "$DEST")"

clone_args=(clone "$REPO_URL" "$DEST")
if [[ -n "$DEPTH" ]]; then
    clone_args+=(--depth "$DEPTH" --branch "$REF" --single-branch)
fi

log "Cloning $REPO_URL -> $DEST (ref=$REF${DEPTH:+, depth=$DEPTH})"
git "${clone_args[@]}"

# For non-shallow clones we still need to check out the requested ref
# (a tag, commit sha, or non-default branch).
if [[ -z "$DEPTH" ]]; then
    git -C "$DEST" fetch --tags --prune origin
    git -C "$DEST" checkout "$REF"
fi

log "Repo at $(git -C "$DEST" rev-parse --short HEAD) ($REF)"
log "Done. Next step:"
log "  cd $DEST && dev_scripts/fetch-assets.sh && dev_scripts/snapshot-test.sh"
