#!/usr/bin/env bash
# SM-SCRUB-3 (negative): a degraded array must refuse `fs scrub start`.
#
# Degrading is done with a REAL `mdadm --fail` against a real 3-disk RAID5,
# independent of shr-rs entirely -- the same "don't trust the tool under
# test to create its own precondition dishonestly" spirit as every other
# case here, applied to the ARRANGE step this time (the design
# SS3.1: preconditions must themselves be observed and recorded).
#
# Judgment is by /sys/block/<md>/md/sync_action BEFORE vs AFTER the refused
# `scrub start` call -- never shr-rs's exit code alone (that's checked too,
# but the exit code is cheap to get right for the wrong reasons; the sysfs
# read is what actually proves no scrub was ever kicked off in the kernel).
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_POINT=/mnt/shr-smoke
RESULT=PASS
FAILURES=()

fail() {
    RESULT=FAIL
    FAILURES+=("$1")
    echo "FAIL: $1" >&2
}

cleanup() {
    sudo umount "$MOUNT_POINT" 2>/dev/null || true
    fixture_down >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== SM-SCRUB-3: arrange (create a real 3-disk RAID5, then genuinely degrade it) =="
if ! fixture_up 16G 16G 16G; then
    echo "SM-SCRUB-3: BLOCKED (fixture_up failed)"
    exit 2
fi
DISKS="ata-LOOP_DISK_10,ata-LOOP_DISK_11,ata-LOOP_DISK_12"

CREATE_OUTPUT="$(sudo "$SHR_RS" --json create --mode shr --disks "$DISKS" --mount "$MOUNT_POINT" 2>&1)"
CREATE_EXIT=$?
echo "$CREATE_OUTPUT"
if [[ "$CREATE_EXIT" -ne 0 ]]; then
    echo "SM-SCRUB-3: BLOCKED (create failed, nothing to degrade)"
    fixture_down >/dev/null 2>&1 || true
    exit 2
fi

MD_NAME="$(smoke_find_md_for_members loop10p1 loop11p1 loop12p1)"
if [[ -z "$MD_NAME" ]]; then
    echo "SM-SCRUB-3: BLOCKED (could not find the real md device for band0's members in /proc/mdstat)"
    exit 2
fi
echo "band0 is /dev/$MD_NAME"

if ! smoke_wait_sync_idle "$MD_NAME"; then
    echo "SM-SCRUB-3: BLOCKED (initial resync never finished within the wait window)"
    exit 2
fi

# Genuinely fail one member -- a real mdadm command against the kernel, not
# anything shr-rs does. This is what "degraded" means for the rest of this
# test: the kernel's own /sys/block/<md>/md/degraded counter, not a state.toml
# flag.
sudo mdadm "/dev/$MD_NAME" --fail /dev/loop11p1
DEGRADED_BEFORE="$(cat "/sys/block/$MD_NAME/md/degraded" 2>/dev/null || echo unknown)"
echo "-- after mdadm --fail --"
cat /proc/mdstat
echo "degraded count: $DEGRADED_BEFORE"
[[ "$DEGRADED_BEFORE" -gt 0 ]] || {
    echo "SM-SCRUB-3: BLOCKED (mdadm --fail did not actually degrade the array -- cannot test the guard)"
    exit 2
}

BEFORE_SYNC_ACTION="$(cat "/sys/block/$MD_NAME/md/sync_action" 2>/dev/null || echo unknown)"
echo "sync_action before scrub attempt: $BEFORE_SYNC_ACTION"

echo "== SM-SCRUB-3: act (scrub start on a degraded array) =="
SCRUB_OUTPUT="$(sudo "$SHR_RS" --json fs scrub start 2>&1)"
SCRUB_EXIT=$?
echo "$SCRUB_OUTPUT"
echo "shr-rs fs scrub start exit code: $SCRUB_EXIT"

echo "== SM-SCRUB-3: assert (independent kernel state observation) =="

[[ "$SCRUB_EXIT" -ne 0 ]] || fail "scrub start should have been refused on a degraded array (exit 0 instead)"

AFTER_SYNC_ACTION="$(cat "/sys/block/$MD_NAME/md/sync_action" 2>/dev/null || echo unknown)"
echo "sync_action after scrub attempt: $AFTER_SYNC_ACTION"
[[ "$AFTER_SYNC_ACTION" == "$BEFORE_SYNC_ACTION" ]] || \
    fail "sync_action changed despite the refusal: before=$BEFORE_SYNC_ACTION after=$AFTER_SYNC_ACTION (a scrub actually started)"
[[ "$AFTER_SYNC_ACTION" == "idle" ]] || fail "sync_action is '$AFTER_SYNC_ACTION', expected 'idle' -- no scrub should ever have run here"

DEGRADED_AFTER="$(cat "/sys/block/$MD_NAME/md/degraded" 2>/dev/null || echo unknown)"
[[ "$DEGRADED_AFTER" == "$DEGRADED_BEFORE" ]] || \
    fail "degraded count changed across the refused scrub attempt: before=$DEGRADED_BEFORE after=$DEGRADED_AFTER"

echo "== SM-SCRUB-3: cleanup + verify teardown (R4) =="
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-SCRUB-3: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-SCRUB-3: PASS"
else
    printf 'SM-SCRUB-3: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
