#!/usr/bin/env bash
# snapshot-test.sh
#
# End-to-end snapshot/restore smoke test for cloud-hypervisor on aarch64 Linux:
#
#   1. (Optionally) install apt build dependencies.
#   2. cargo build --release  (cloud-hypervisor + ch-remote).
#   3. Boot a VM (direct kernel boot, jammy cloud image).
#   4. Pause + snapshot the VM.
#   5. Tear down the source VMM.
#   6. Boot a new VMM from the snapshot with resume=on.
#   7. Verify the restored VMM reaches the Running state and that the expected
#      "restored" / "resumed" events are emitted.
#   8. Clean up sockets, snapshot dir, logs, and any lingering processes.
#
# Exit codes:
#     0  snapshot boot passed
#     1  snapshot boot failed (restore didn't reach Running / events missing)
#     2  preflight / build / setup error
#
# Usage: dev_scripts/snapshot-test.sh [options]   (run from any CWD)

set -euo pipefail

# ---- Defaults ----------------------------------------------------------------
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKLOADS_DIR="${WORKLOADS_DIR:-$HOME/workloads}"
SNAPSHOT_DIR="${SNAPSHOT_DIR:-/tmp/ch-snapshot-test}"
SOCK_ORIG="${SOCK_ORIG:-/tmp/ch-snapshot-test-orig.sock}"
SOCK_REST="${SOCK_REST:-/tmp/ch-snapshot-test-restored.sock}"
EVENTS_ORIG="${EVENTS_ORIG:-/tmp/ch-snapshot-test-orig-events.json}"
EVENTS_REST="${EVENTS_REST:-/tmp/ch-snapshot-test-restored-events.json}"
LOG_ORIG="${LOG_ORIG:-/tmp/ch-snapshot-test-orig.log}"
LOG_REST="${LOG_REST:-/tmp/ch-snapshot-test-restored.log}"
CLOUDINIT_IMG="${CLOUDINIT_IMG:-/tmp/ch-snapshot-test-cloudinit.img}"

KERNEL_NAME="Image-arm64"
DISK_NAME="jammy-server-cloudimg-arm64.raw"

BOOT_TIMEOUT_SEC=60
EVENT_TIMEOUT_SEC=30

SKIP_BUILD=0
SKIP_APT=0
KEEP_LOGS=0
KEEP_ARTIFACTS=0

ORIG_PID=""
REST_PID=""
FAILED=0

# ---- Helpers -----------------------------------------------------------------
log()  { printf '[snapshot-test] %s\n' "$*"; }
err()  { printf '[snapshot-test] ERROR: %s\n' "$*" >&2; }
die()  { err "$*"; exit 2; }

usage() {
    cat <<EOF
Usage: $(basename "$0") [options]

Options:
  --workloads-dir DIR   Directory with pre-fetched assets (default: \$HOME/workloads)
  --skip-build          Skip 'cargo build --release' (use existing target/release)
  --skip-apt            Skip the automatic apt install of build dependencies
  --keep-logs           Do not delete VMM stdout/stderr logs at the end
  --keep-artifacts      Do not delete sockets, snapshot dir, event logs, cloud-init img
  -h, --help            Show this help
EOF
}

# ---- Arg parsing -------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --workloads-dir)  WORKLOADS_DIR="$2"; shift 2 ;;
        --skip-build)     SKIP_BUILD=1; shift ;;
        --skip-apt)       SKIP_APT=1; shift ;;
        --keep-logs)      KEEP_LOGS=1; shift ;;
        --keep-artifacts) KEEP_ARTIFACTS=1; shift ;;
        -h|--help)        usage; exit 0 ;;
        *)                err "unknown argument: $1"; usage >&2; exit 2 ;;
    esac
done

CH_BIN="$REPO_ROOT/target/release/cloud-hypervisor"
CH_REMOTE="$REPO_ROOT/target/release/ch-remote"

# ---- Cleanup trap ------------------------------------------------------------
cleanup() {
    local rc=$?
    log "Cleanup starting (exit code so far: $rc)"

    # Try a graceful shutdown of both VMMs, then force-kill if needed.
    if [[ -n "${REST_PID:-}" ]] && kill -0 "$REST_PID" 2>/dev/null; then
        if [[ -S "$SOCK_REST" ]]; then
            "$CH_REMOTE" --api-socket "$SOCK_REST" shutdown-vmm >/dev/null 2>&1 || true
        fi
        sleep 1
        kill -9 "$REST_PID" 2>/dev/null || true
        wait "$REST_PID" 2>/dev/null || true
    fi
    if [[ -n "${ORIG_PID:-}" ]] && kill -0 "$ORIG_PID" 2>/dev/null; then
        if [[ -S "$SOCK_ORIG" ]]; then
            "$CH_REMOTE" --api-socket "$SOCK_ORIG" shutdown-vmm >/dev/null 2>&1 || true
        fi
        sleep 1
        kill -9 "$ORIG_PID" 2>/dev/null || true
        wait "$ORIG_PID" 2>/dev/null || true
    fi

    if [[ $KEEP_ARTIFACTS -eq 0 ]]; then
        rm -f "$SOCK_ORIG" "$SOCK_REST"
        rm -f "$EVENTS_ORIG" "$EVENTS_REST"
        rm -f "$CLOUDINIT_IMG"
        rm -rf "$SNAPSHOT_DIR"
    else
        log "Keeping artifacts per --keep-artifacts"
    fi

    # Keep logs on failure (or if requested) so the user can debug.
    if [[ $KEEP_LOGS -eq 0 && $FAILED -eq 0 ]]; then
        rm -f "$LOG_ORIG" "$LOG_REST"
    else
        log "Logs retained: $LOG_ORIG $LOG_REST"
    fi

    log "Cleanup done."
}
trap cleanup EXIT

# ---- Preflight ---------------------------------------------------------------
preflight() {
    log "Preflight checks"

    local arch
    arch="$(uname -m)"
    if [[ "$arch" != "aarch64" ]]; then
        die "this script targets aarch64 Linux; detected: $arch"
    fi

    if [[ ! -r /dev/kvm || ! -w /dev/kvm ]]; then
        die "/dev/kvm is not accessible by the current user (need rw). Try: sudo usermod -aG kvm $USER"
    fi

    if [[ ! -f "$WORKLOADS_DIR/$KERNEL_NAME" ]]; then
        die "missing kernel: $WORKLOADS_DIR/$KERNEL_NAME (run dev_scripts/fetch-assets.sh first)"
    fi
    if [[ ! -f "$WORKLOADS_DIR/$DISK_NAME" ]]; then
        die "missing disk: $WORKLOADS_DIR/$DISK_NAME (run dev_scripts/fetch-assets.sh first)"
    fi

    if ! command -v cargo >/dev/null 2>&1; then
        die "cargo not found on PATH. Install Rust (rustup) first: https://rustup.rs"
    fi
}

# ---- Apt deps ----------------------------------------------------------------
install_apt_deps() {
    if [[ $SKIP_APT -eq 1 ]]; then
        log "Skipping apt install (per --skip-apt)"
        return 0
    fi
    log "Installing apt build dependencies (sudo required)"
    sudo apt-get update
    sudo apt-get install -y \
        git build-essential m4 bison flex uuid-dev qemu-utils musl-tools \
        libssl-dev pkg-config libcap2-bin dosfstools mtools jq
}

# ---- Build -------------------------------------------------------------------
build_ch() {
    if [[ $SKIP_BUILD -eq 1 ]]; then
        log "Skipping build (per --skip-build)"
    else
        log "Building cloud-hypervisor (cargo build --release)"
        ( cd "$REPO_ROOT" && cargo build --release )
    fi

    [[ -x "$CH_BIN"    ]] || die "missing binary: $CH_BIN"
    [[ -x "$CH_REMOTE" ]] || die "missing binary: $CH_REMOTE"

    # Idempotent setcap so the binary can create tap devices if ever used.
    # Harmless when no network is configured.
    if command -v setcap >/dev/null 2>&1; then
        sudo setcap cap_net_admin+ep "$CH_BIN" || true
    fi
}

# ---- Asset prep --------------------------------------------------------------
prepare_assets() {
    log "Preparing cloud-init seed image"
    ( cd "$REPO_ROOT" && scripts/create-cloud-init.sh -o "$CLOUDINIT_IMG" )

    mkdir -p "$SNAPSHOT_DIR"
    # Snapshot dir must be empty for a fresh run.
    rm -rf "${SNAPSHOT_DIR:?}"/*
    rm -f "$SOCK_ORIG" "$SOCK_REST" "$EVENTS_ORIG" "$EVENTS_REST"
}

# ---- API / event polling -----------------------------------------------------
wait_for_socket() {
    local sock="$1" timeout="$2" elapsed=0
    while [[ $elapsed -lt $timeout ]]; do
        if [[ -S "$sock" ]] && "$CH_REMOTE" --api-socket "$sock" info >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done
    return 1
}

wait_for_state() {
    # Wait until `ch-remote info` reports the given VM state (e.g. "Running", "Paused").
    local sock="$1" want_state="$2" timeout="$3" elapsed=0 out=""
    while [[ $elapsed -lt $timeout ]]; do
        if out="$("$CH_REMOTE" --api-socket "$sock" info 2>/dev/null)"; then
            # jq path: .state -> "Running" / "Paused" / ...
            local state
            state="$(printf '%s' "$out" | jq -r '.state // empty' 2>/dev/null || true)"
            if [[ "$state" == "$want_state" ]]; then
                return 0
            fi
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done
    err "timed out waiting for VM state=$want_state on $sock (last info: $out)"
    return 1
}

wait_for_event() {
    # Grep the event-monitor JSON lines for a matching {"source":..., "event":...} pair.
    local events_file="$1" source_name="$2" event_name="$3" timeout="$4" elapsed=0
    while [[ $elapsed -lt $timeout ]]; do
        if [[ -f "$events_file" ]] && \
           jq -e --arg s "$source_name" --arg e "$event_name" \
               'select(.source == $s and .event == $e)' \
               "$events_file" >/dev/null 2>&1; then
            return 0
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done
    err "timed out waiting for event source=$source_name event=$event_name in $events_file"
    return 1
}

# ---- Boot original VM --------------------------------------------------------
boot_original() {
    log "Booting original VM"
    "$CH_BIN" \
        --api-socket  "$SOCK_ORIG" \
        --event-monitor path="$EVENTS_ORIG" \
        --cpus  boot=2 \
        --memory size=1G \
        --kernel "$WORKLOADS_DIR/$KERNEL_NAME" \
        --cmdline "root=/dev/vda1 console=hvc0 rw systemd.journald.forward_to_console=1" \
        --disk path="$WORKLOADS_DIR/$DISK_NAME" path="$CLOUDINIT_IMG" \
        --serial off --console off \
        >"$LOG_ORIG" 2>&1 &
    ORIG_PID=$!
    log "  pid=$ORIG_PID, socket=$SOCK_ORIG, events=$EVENTS_ORIG, log=$LOG_ORIG"

    if ! wait_for_socket "$SOCK_ORIG" "$BOOT_TIMEOUT_SEC"; then
        die "original VMM API socket did not come up within ${BOOT_TIMEOUT_SEC}s"
    fi
    if ! wait_for_state "$SOCK_ORIG" "Running" "$BOOT_TIMEOUT_SEC"; then
        die "original VM did not reach Running state"
    fi
    log "  original VM is Running"
}

# ---- Snapshot ----------------------------------------------------------------
take_snapshot() {
    log "Pausing original VM"
    "$CH_REMOTE" --api-socket "$SOCK_ORIG" pause
    if ! wait_for_event "$EVENTS_ORIG" "vm" "paused" "$EVENT_TIMEOUT_SEC"; then
        die "did not observe vm:paused event"
    fi

    log "Snapshotting to $SNAPSHOT_DIR"
    "$CH_REMOTE" --api-socket "$SOCK_ORIG" snapshot "file://$SNAPSHOT_DIR"
    if ! wait_for_event "$EVENTS_ORIG" "vm" "snapshotted" "$EVENT_TIMEOUT_SEC"; then
        die "did not observe vm:snapshotted event"
    fi

    # Expected artifacts: config.json, state.json, and at least one memory region file.
    [[ -f "$SNAPSHOT_DIR/config.json" ]] || die "snapshot missing config.json"
    [[ -f "$SNAPSHOT_DIR/state.json"  ]] || die "snapshot missing state.json"
    if ! ls "$SNAPSHOT_DIR"/memory-* >/dev/null 2>&1; then
        die "snapshot missing memory-* file(s)"
    fi
    log "  snapshot artifacts OK: $(ls "$SNAPSHOT_DIR" | tr '\n' ' ')"
}

# ---- Teardown original -------------------------------------------------------
teardown_original() {
    log "Tearing down original VMM"
    "$CH_REMOTE" --api-socket "$SOCK_ORIG" shutdown-vmm || true

    local elapsed=0
    while kill -0 "$ORIG_PID" 2>/dev/null && [[ $elapsed -lt 15 ]]; do
        sleep 1
        elapsed=$((elapsed + 1))
    done
    if kill -0 "$ORIG_PID" 2>/dev/null; then
        log "  original VMM did not exit in ${elapsed}s; sending SIGKILL"
        kill -9 "$ORIG_PID" 2>/dev/null || true
    fi
    wait "$ORIG_PID" 2>/dev/null || true
    ORIG_PID=""
    rm -f "$SOCK_ORIG"
    log "  original VMM gone"
}

# ---- Restore + verify --------------------------------------------------------
restore_and_verify() {
    log "Restoring from snapshot (resume=on)"
    "$CH_BIN" \
        --api-socket "$SOCK_REST" \
        --event-monitor path="$EVENTS_REST" \
        --restore "source_url=file://$SNAPSHOT_DIR,resume=on" \
        >"$LOG_REST" 2>&1 &
    REST_PID=$!
    log "  pid=$REST_PID, socket=$SOCK_REST, events=$EVENTS_REST, log=$LOG_REST"

    if ! wait_for_socket "$SOCK_REST" "$BOOT_TIMEOUT_SEC"; then
        err "restored VMM API socket did not come up within ${BOOT_TIMEOUT_SEC}s"
        return 1
    fi
    if ! wait_for_state "$SOCK_REST" "Running" "$BOOT_TIMEOUT_SEC"; then
        err "restored VM did not reach Running state"
        return 1
    fi
    if ! wait_for_event "$EVENTS_REST" "vm" "restored" "$EVENT_TIMEOUT_SEC"; then
        err "did not observe vm:restored event"
        return 1
    fi
    if ! wait_for_event "$EVENTS_REST" "vm" "resumed" "$EVENT_TIMEOUT_SEC"; then
        err "did not observe vm:resumed event"
        return 1
    fi
    log "  restored VM is Running; restored+resumed events observed"
    return 0
}

# ---- Main --------------------------------------------------------------------
main() {
    preflight
    install_apt_deps
    build_ch
    prepare_assets

    boot_original
    take_snapshot
    teardown_original

    if restore_and_verify; then
        log ""
        log "SNAPSHOT BOOT: PASS"
        exit 0
    else
        FAILED=1
        log ""
        err "SNAPSHOT BOOT: FAIL"
        err "--- tail of $LOG_ORIG ---"
        tail -n 50 "$LOG_ORIG" >&2 || true
        err "--- tail of $LOG_REST ---"
        tail -n 50 "$LOG_REST" >&2 || true
        err "--- tail of $EVENTS_REST ---"
        tail -n 50 "$EVENTS_REST" >&2 || true
        exit 1
    fi
}

main "$@"
