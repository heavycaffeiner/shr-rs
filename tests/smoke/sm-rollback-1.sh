#!/usr/bin/env bash
# mid-pipeline failure leaves no trace (D10), verified against
# real kernel/LVM state.
#
# Before Step 8, this guest had no Btrfs support, so the ONLY reachable
# failure was D11's prerequisite check rejecting the whole request before
# any destructive command ran at all -- useful, but it only proved the
# "nothing started yet" half of D10, never real rollback of a partition/
# mdadm/LVM sequence that had already made progress. Now that Step 8 has put
# a real Btrfs kernel module on this guest, the pipeline can be driven almost
# all the way to its LAST step and forced to fail for real: the mount target
# is deliberately pre-created as a plain FILE, not a directory.
# `create()` runs `mkdir -p` through `CommandRunner` (not a raw, `.ok()`-
# swallowed `std::fs::create_dir_all`), so the failure now surfaces at that
# `mkdir -p` call -- still AFTER mdadm/pvcreate/vgcreate/lvcreate/mkfs.btrfs
# have all already succeeded for real, just one step earlier than the old
# `mount` ENOTDIR. That still exercises the full rollback journal
# (UndoAction::RemoveLv/RemoveVg/RemovePv/TeardownArray/RemovePartition, in
# that order) instead of only D11's early-exit path.
#
# D11's early-exit half (a prerequisite check blocking BEFORE any destructive
# command) remains covered separately by
# `crates/shr-exec/tests/exec.rs`/`crates/shr-orchestrate/tests/orchestrate.rs`
# (`ensure_supported` unit tests, `mdadm_create_failure_rolls_back_the_partitions_just_created`,
# `vgcreate_failure_rolls_back_mdadm_array_and_partitions`) -- this script's
# job is specifically the real, full-depth rollback that was unreachable on
# this guest before Step 8.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
# A regular file, not a directory -- `create()`'s `mkdir -p` fails on
# it deterministically ("File exists", the target is not a directory) after
# every earlier destructive step has already succeeded for real.
BOGUS_MOUNT_POINT=/tmp/shr-smoke-not-a-directory
RESULT=PASS
FAILURES=()

fail() {
    RESULT=FAIL
    FAILURES+=("$1")
    echo "FAIL: $1" >&2
}

cleanup() {
    sudo rm -f "$BOGUS_MOUNT_POINT" 2>/dev/null || true
    fixture_down >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== SM-ROLLBACK-1: arrange =="
if ! fixture_up 16G 16G 16G; then
    echo "SM-ROLLBACK-1: BLOCKED (fixture_up failed)"
    exit 2
fi
DISKS="ata-LOOP_DISK_10,ata-LOOP_DISK_11,ata-LOOP_DISK_12"
sudo rm -f "$BOGUS_MOUNT_POINT"
sudo touch "$BOGUS_MOUNT_POINT"

BEFORE_MDSTAT="$(cat /proc/mdstat)"
BEFORE_LSBLK="$(lsblk -no NAME /dev/loop10 /dev/loop11 /dev/loop12)"
echo "-- before --"
echo "$BEFORE_MDSTAT"
echo "$BEFORE_LSBLK"

echo "== SM-ROLLBACK-1: act =="
OUTPUT="$(sudo "$SHR_RS" --json create --mode shr --disks "$DISKS" --mount "$BOGUS_MOUNT_POINT" 2>&1)"
CREATE_EXIT=$?
echo "$OUTPUT"
echo "shr-rs create exit code: $CREATE_EXIT"

echo "== SM-ROLLBACK-1: assert (independent kernel/LVM state observation) =="

[[ "$CREATE_EXIT" -ne 0 ]] || fail "create should have failed (mount target is a file, not a directory)"

AFTER_MDSTAT="$(cat /proc/mdstat)"
AFTER_LSBLK="$(lsblk -no NAME /dev/loop10 /dev/loop11 /dev/loop12)"
echo "-- after --"
echo "$AFTER_MDSTAT"
echo "$AFTER_LSBLK"

[[ "$AFTER_MDSTAT" == *"unused devices: <none>"* ]] || fail "an md array is still present after rollback: $AFTER_MDSTAT"
echo "$AFTER_MDSTAT" | grep -qE '^md[0-9]+ :' && fail "an md<N> array line is still present after rollback"

[[ "$BEFORE_LSBLK" == "$AFTER_LSBLK" ]] || fail "lsblk changed and did not return to its pre-test shape after rollback:
before: $BEFORE_LSBLK
after:  $AFTER_LSBLK"

# Scoped to what THIS test's own `create` call could have made (an md-backed
# PV, VG `shr_vg`, LV `data` -- the CLI's defaults, since no --vg-name/
# --lv-name is passed above), not a global "zero PVs/VGs/LVs on the host"
# count: this guest's root disk is a stock Rocky GenericCloud cloud-init
# image (single XFS partition, no LVM) so a global-zero check happens to
# hold today, but asserting it anyway would make this case fail for a
# reason that has nothing to do with shr-rs's rollback the moment that
# environment assumption stops being true -- the same "adjacent, not the
# same thing" trap as reading a tool's own exit code for kernel state.
PV_COUNT="$(sudo pvs --noheadings -o pv_name 2>/dev/null | grep -c '/dev/md' || true)"
[[ "$PV_COUNT" == 0 ]] || fail "$PV_COUNT physical volume(s) over an md device still present after rollback"
sudo vgs --noheadings -o vg_name 2>/dev/null | grep -qw shr_vg && fail "VG shr_vg still present after rollback"
sudo lvs --noheadings -o lv_name 2>/dev/null | grep -qw data && fail "LV data still present after rollback"

[[ -f /var/lib/shr-rs/state.toml ]] && fail "state.toml was persisted despite create() failing"

echo "== SM-ROLLBACK-1: cleanup + verify teardown (R4) =="
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-ROLLBACK-1: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-ROLLBACK-1: PASS"
else
    printf 'SM-ROLLBACK-1: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
