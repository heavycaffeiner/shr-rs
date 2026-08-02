#!/usr/bin/env bash
# SM-REPLACE-2 (negative): `disk replace` must refuse a smaller replacement
# disk, leaving the array untouched.
#
# `OrchestrationEngine::replace_disk` (crates/shr-orchestrate/src/engine.rs)
# checks `new_disk.size_bytes < old_disk.size_bytes` up front and returns a
# `Validation` error before issuing any mdadm command at all. Judgment here
# is /proc/mdstat's member list and raid_disks/degraded count before vs.
# after the refused call -- never shr-rs's exit code alone, and specifically
# never trusting that "no destructive command was issued" just because the
# tool SAYS it refused early: the array's real member list must still be
# the original 3 loop10/11/12 partitions, with the smaller replacement disk
# nowhere in it.
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

echo "== SM-REPLACE-2: arrange (a real 3-disk RAID5; a genuinely SMALLER 4th"
echo "   disk is the rejected replacement candidate) =="
if ! fixture_up 16G 16G 16G 8G; then
    echo "SM-REPLACE-2: BLOCKED (fixture_up failed)"
    exit 2
fi
DISKS="ata-LOOP_DISK_10,ata-LOOP_DISK_11,ata-LOOP_DISK_12"

CREATE_OUTPUT="$(sudo "$SHR_RS" --json create --mode shr --disks "$DISKS" --mount "$MOUNT_POINT" 2>&1)"
CREATE_EXIT=$?
echo "$CREATE_OUTPUT"
if [[ "$CREATE_EXIT" -ne 0 ]]; then
    echo "SM-REPLACE-2: BLOCKED (create failed)"
    fixture_down >/dev/null 2>&1 || true
    exit 2
fi

MD_NAME="$(smoke_find_md_for_members loop10p1 loop11p1 loop12p1)"
if [[ -z "$MD_NAME" ]]; then
    echo "SM-REPLACE-2: BLOCKED (could not find the real md device for band0's members)"
    exit 2
fi
echo "band0 is /dev/$MD_NAME"

if ! smoke_wait_sync_idle "$MD_NAME"; then
    echo "SM-REPLACE-2: BLOCKED (initial resync never finished within the wait window)"
    exit 2
fi

# Independent confirmation, straight from the kernel, that loop13 really is
# smaller than the member it's being asked to replace -- if this were ever
# false the rest of the test would be meaningless.
OLD_SIZE_SECTORS="$(cat /sys/block/loop10/size 2>/dev/null || echo 0)"
NEW_SIZE_SECTORS="$(cat /sys/block/loop13/size 2>/dev/null || echo 0)"
echo "loop10 (being replaced) size: $OLD_SIZE_SECTORS sectors; loop13 (candidate) size: $NEW_SIZE_SECTORS sectors"
[[ "$NEW_SIZE_SECTORS" -lt "$OLD_SIZE_SECTORS" ]] || {
    echo "SM-REPLACE-2: BLOCKED (fixture sizing is wrong -- loop13 is not actually smaller than loop10)"
    exit 2
}

BEFORE_MDSTAT="$(cat /proc/mdstat)"
BEFORE_MEMBERS="$(grep "^${MD_NAME} :" /proc/mdstat | grep -oE 'loop[0-9]+p[0-9]+\[[0-9]+\]' | sort)"
BEFORE_RAID_DISKS="$(cat "/sys/block/$MD_NAME/md/raid_disks" 2>/dev/null || echo 0)"
BEFORE_DEGRADED="$(cat "/sys/block/$MD_NAME/md/degraded" 2>/dev/null || echo unknown)"
echo "-- before replace --"
echo "$BEFORE_MDSTAT"
echo "members: $BEFORE_MEMBERS"

echo "== SM-REPLACE-2: act (disk replace with a smaller disk) =="
REPLACE_OUTPUT="$(sudo "$SHR_RS" --json disk replace --old ata-LOOP_DISK_10 --new ata-LOOP_DISK_13 --yes 2>&1)"
REPLACE_EXIT=$?
echo "$REPLACE_OUTPUT"
echo "disk replace exit code: $REPLACE_EXIT"

echo "== SM-REPLACE-2: assert (independent kernel state observation) =="

[[ "$REPLACE_EXIT" -ne 0 ]] || fail "disk replace should have been refused for a smaller replacement disk (exit 0 instead)"

AFTER_MDSTAT="$(cat /proc/mdstat)"
AFTER_MEMBERS="$(grep "^${MD_NAME} :" /proc/mdstat | grep -oE 'loop[0-9]+p[0-9]+\[[0-9]+\]' | sort)"
AFTER_RAID_DISKS="$(cat "/sys/block/$MD_NAME/md/raid_disks" 2>/dev/null || echo 0)"
AFTER_DEGRADED="$(cat "/sys/block/$MD_NAME/md/degraded" 2>/dev/null || echo unknown)"
echo "-- after refused replace --"
echo "$AFTER_MDSTAT"
echo "members: $AFTER_MEMBERS"

[[ "$AFTER_MEMBERS" == "$BEFORE_MEMBERS" ]] || fail "band0's member list changed across the refused replace:
before: $BEFORE_MEMBERS
after:  $AFTER_MEMBERS"
[[ "$AFTER_RAID_DISKS" == "$BEFORE_RAID_DISKS" ]] || \
    fail "raid_disks changed across the refused replace: before=$BEFORE_RAID_DISKS after=$AFTER_RAID_DISKS"
[[ "$AFTER_DEGRADED" == "$BEFORE_DEGRADED" ]] || \
    fail "degraded count changed across the refused replace: before=$BEFORE_DEGRADED after=$AFTER_DEGRADED"
echo "$AFTER_MDSTAT" | grep -q "loop13p1" && fail "the smaller replacement disk (loop13) was actually added as a member -- replace should have refused before touching anything"

echo "== SM-REPLACE-2: cleanup + verify teardown (R4) =="
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-REPLACE-2: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-REPLACE-2: PASS"
else
    printf 'SM-REPLACE-2: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
