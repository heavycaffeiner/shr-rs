#!/usr/bin/env bash
# SM-THROTTLE-4: an unreadable signal must not decay a `--priority max`
# operation.
#
# D2, measured on a real host: `max` sets its contention thresholds to
# saturating values precisely so `over_threshold` can never trip, but the
# adjacent "any signal unreadable" branch was not profile-aware and returned
# `Decrease(0.7)` regardless. On a host where `smartctl` is absent -- or the
# disks sit behind a controller needing an explicit `-d`, or the drive
# reports no temperature attribute -- a `--priority max` reshape therefore
# decayed by 0.7 every two minutes and reached the floor within half an hour.
# The profile whose entire purpose is "do not brake this" braked to the
# slowest setting the system offers.
#
# The fix has two halves and this case exercises both at once: an unreadable
# contention signal holds under `max`, and an unreadable safety-critical one
# still decreases under every profile -- but `max`'s own floor is the
# unbounded sentinel, so that decrease cannot actually slow it down.
#
# `smartctl` is made unavailable honestly, by moving the binary aside for the
# duration and putting it back in the exit trap. Loopback devices have no
# SMART identity anyway, so this is the same reading either way; what it
# guarantees is that the reading is unavailable for a reason this script
# controls rather than one the fixture happens to have.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_POINT=/mnt/shr-smoke
TICKS=3
SMARTCTL_PATH=""
SMARTCTL_STASH=/tmp/sm-throttle-4-smartctl
RESULT=PASS
FAILURES=()

fail() {
    RESULT=FAIL
    FAILURES+=("$1")
    echo "FAIL: $1" >&2
}

restore_smartctl() {
    if [[ -n "$SMARTCTL_PATH" && -e "$SMARTCTL_STASH" ]]; then
        sudo mv "$SMARTCTL_STASH" "$SMARTCTL_PATH" 2>/dev/null || true
    fi
}

cleanup() {
    restore_smartctl
    sudo umount "$MOUNT_POINT" 2>/dev/null || true
    fixture_down >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== SM-THROTTLE-4: arrange (a real 2-disk RAID1, promoted by a real expand) =="
if ! fixture_up 16G 16G 16G; then
    echo "SM-THROTTLE-4: BLOCKED (fixture_up failed)"
    exit 2
fi

CREATE_OUTPUT="$(sudo "$SHR_RS" --json create --mode shr \
    --disks ata-LOOP_DISK_10,ata-LOOP_DISK_11 --mount "$MOUNT_POINT" 2>&1)"
CREATE_EXIT=$?
echo "$CREATE_OUTPUT"
if [[ "$CREATE_EXIT" -ne 0 ]]; then
    echo "SM-THROTTLE-4: BLOCKED (create failed)"
    exit 2
fi

MD_NAME="$(smoke_find_md_for_members loop10p1 loop11p1)"
if [[ -z "$MD_NAME" ]]; then
    echo "SM-THROTTLE-4: BLOCKED (could not find the real md device)"
    exit 2
fi
if ! smoke_wait_sync_idle "$MD_NAME"; then
    echo "SM-THROTTLE-4: BLOCKED (initial resync never finished within the wait window)"
    exit 2
fi

echo "== SM-THROTTLE-4: make smartctl genuinely unavailable =="
SMARTCTL_PATH="$(command -v smartctl 2>/dev/null || true)"
if [[ -n "$SMARTCTL_PATH" ]]; then
    sudo mv "$SMARTCTL_PATH" "$SMARTCTL_STASH"
    echo "moved $SMARTCTL_PATH aside for the duration"
else
    echo "smartctl is not installed on this host -- already the condition under test"
fi
command -v smartctl >/dev/null 2>&1 && \
    echo "SM-THROTTLE-4: BLOCKED (smartctl is still on PATH; the condition was not established)" && exit 2

echo "== SM-THROTTLE-4: act (expand --priority max, then $TICKS throttle ticks) =="
# `--skip-scrub-check`: the group was created seconds ago, so the
# scrub-freshness gate correctly refuses the expand and no reshape would
# start at all. That gate is SM-SCRUB-4's subject.
EXPAND_OUTPUT="$(sudo "$SHR_RS" --json expand --add ata-LOOP_DISK_12 --priority max --skip-scrub-check 2>&1)"
EXPAND_EXIT=$?
echo "$EXPAND_OUTPUT"
if [[ "$EXPAND_EXIT" -ne 0 ]]; then
    echo "SM-THROTTLE-4: BLOCKED (expand --priority max did not succeed)"
    exit 2
fi
if ! smoke_wait_sync_action "$MD_NAME" reshape 12 5; then
    echo "SM-THROTTLE-4: BLOCKED (sync_action never reached 'reshape' -- nothing to throttle)"
    exit 2
fi

BEFORE_MAX="$(smoke_sync_limit "$MD_NAME" max)"
BEFORE_MIN="$(smoke_sync_limit "$MD_NAME" min)"
echo "limits right after the reshape started: max=$BEFORE_MAX min=$BEFORE_MIN"

for tick in $(seq 1 "$TICKS"); do
    sudo "$SHR_RS" --json internal reshape-throttle-tick >/dev/null 2>&1 || \
        fail "throttle tick $tick failed"
    echo "tick $tick: max=$(smoke_sync_limit "$MD_NAME" max) min=$(smoke_sync_limit "$MD_NAME" min)"
done

echo "== SM-THROTTLE-4: assert (independent kernel state observation) =="
AFTER_MAX="$(smoke_sync_limit "$MD_NAME" max)"
AFTER_MIN="$(smoke_sync_limit "$MD_NAME" min)"
echo "limits after $TICKS ticks with no readable SMART: max=$AFTER_MAX min=$AFTER_MIN"

[[ "$AFTER_MAX" == "$BEFORE_MAX" ]] || \
    fail "sync_speed_max moved from $BEFORE_MAX to $AFTER_MAX under --priority max with unreadable telemetry -- the profile whose whole purpose is 'do not brake this' braked on a signal it does not consult"
[[ "$AFTER_MIN" == "$BEFORE_MIN" ]] || \
    fail "sync_speed_min moved from $BEFORE_MIN to $AFTER_MIN under --priority max"

# And the reshape is genuinely still running, so this is not "nothing
# changed because nothing was happening".
STILL="$(cat "/sys/block/$MD_NAME/md/sync_action" 2>/dev/null || echo unknown)"
[[ "$STILL" == "reshape" ]] || \
    fail "sync_action is '$STILL', so the ticks above had no running reshape to govern and the assertions prove nothing"

echo "== SM-THROTTLE-4: cleanup + verify teardown (R4) =="
restore_smartctl
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-THROTTLE-4: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-THROTTLE-4: PASS"
else
    printf 'SM-THROTTLE-4: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
