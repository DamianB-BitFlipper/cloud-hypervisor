#!/usr/bin/env bash
# fetch-assets.sh
#
# One-time download of static assets required to boot cloud-hypervisor on
# aarch64 Linux for the snapshot/restore test. Idempotent: existing files are
# kept unless --force is given.
#
# Assets fetched into $WORKLOADS_DIR (default $HOME/workloads):
#   * Image-arm64                            (CH custom Linux kernel, aarch64)
#   * jammy-server-cloudimg-arm64.raw        (Ubuntu 22.04 cloud image, raw)
#   * CLOUDHV_EFI.fd                         (edk2 UEFI firmware, aarch64)
#
# Usage: dev_scripts/fetch-assets.sh [--workloads-dir DIR] [--force]

set -euo pipefail

# ---- Pinned URLs and expected SHA1 sums --------------------------------------
KERNEL_URL="https://github.com/cloud-hypervisor/linux/releases/download/ch-release-v6.16.9-20251112/Image-arm64"
KERNEL_NAME="Image-arm64"
KERNEL_SHA1=""   # upstream does not publish a pinned sum; leave empty to skip

DISK_URL="https://ch-images.azureedge.net/jammy-server-cloudimg-arm64-custom-20220329-0.raw"
DISK_NAME="jammy-server-cloudimg-arm64.raw"
DISK_SHA1="1f2b71be43b8f748f01306c4454e5c921343faa4"

FW_URL="https://github.com/cloud-hypervisor/edk2/releases/download/ch-1e1b96f126/CLOUDHV_EFI.fd"
FW_NAME="CLOUDHV_EFI.fd"
FW_SHA1="ce3656987f9e4238ef8afbd65fca219460c1f767"

# ---- Defaults ----------------------------------------------------------------
WORKLOADS_DIR="${WORKLOADS_DIR:-$HOME/workloads}"
FORCE=0

usage() {
    cat <<EOF
Usage: $(basename "$0") [options]

Options:
  --workloads-dir DIR   Directory to download assets into (default: \$HOME/workloads)
  --force               Re-download even if the file already exists
  -h, --help            Show this help

Environment variables:
  WORKLOADS_DIR         Same as --workloads-dir
EOF
}

# ---- Arg parsing -------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --workloads-dir)
            WORKLOADS_DIR="$2"
            shift 2
            ;;
        --force)
            FORCE=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "ERROR: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

# ---- Helpers -----------------------------------------------------------------
log()  { printf '[fetch-assets] %s\n' "$*"; }
die()  { printf '[fetch-assets] ERROR: %s\n' "$*" >&2; exit 1; }

require_cmd() {
    local cmd="$1" pkg="$2"
    if ! command -v "$cmd" >/dev/null 2>&1; then
        log "Missing '$cmd'. Attempting: sudo apt-get install -y $pkg"
        sudo apt-get update
        sudo apt-get install -y "$pkg"
    fi
}

verify_sha1() {
    local path="$1" expected="$2"
    if [[ -z "$expected" ]]; then
        log "  (no pinned sha1 for $(basename "$path"); skipping verification)"
        return 0
    fi
    local actual
    actual="$(sha1sum "$path" | awk '{print $1}')"
    if [[ "$actual" != "$expected" ]]; then
        die "sha1 mismatch for $(basename "$path"): expected=$expected actual=$actual"
    fi
    log "  sha1 OK"
}

download() {
    local url="$1" dest="$2" expected_sha1="$3"
    if [[ -f "$dest" && $FORCE -eq 0 ]]; then
        log "Already present: $dest"
        verify_sha1 "$dest" "$expected_sha1"
        return 0
    fi
    log "Downloading: $url"
    log "         ->  $dest"
    # -nc would skip existing; we removed that by checking above. Use -O for exact name.
    # --tries for resilience over flaky networks.
    wget --tries=3 --show-progress -O "$dest.partial" "$url"
    mv -f "$dest.partial" "$dest"
    verify_sha1 "$dest" "$expected_sha1"
}

# ---- Main --------------------------------------------------------------------
main() {
    log "Workloads dir: $WORKLOADS_DIR"
    mkdir -p "$WORKLOADS_DIR"

    require_cmd wget     wget
    require_cmd sha1sum  coreutils

    download "$KERNEL_URL" "$WORKLOADS_DIR/$KERNEL_NAME" "$KERNEL_SHA1"
    download "$DISK_URL"   "$WORKLOADS_DIR/$DISK_NAME"   "$DISK_SHA1"
    download "$FW_URL"     "$WORKLOADS_DIR/$FW_NAME"     "$FW_SHA1"

    log ""
    log "All assets present in $WORKLOADS_DIR:"
    ls -lh "$WORKLOADS_DIR/$KERNEL_NAME" "$WORKLOADS_DIR/$DISK_NAME" "$WORKLOADS_DIR/$FW_NAME" \
        | awk '{printf "  %-40s  %s\n", $NF, $5}'
    log "Done."
}

main "$@"
