#!/usr/bin/env bash
# an expansion interrupted mid-reshape must be resumable, must
# reach the target layout, and must not lose data.
#
# A note on what "kill the process" honestly means here: `shr-rs expand`
# issues `mdadm --grow` and then RETURNS almost immediately -- the reshape
# itself runs as a kernel thread, not as anything the `shr-rs` CLI process
# stays alive to drive (see execute_grow's own `resize_pending` doc comment
# in crates/shr-orchestrate/src/engine.rs: "expand() must return promptly...
# blocking here would freeze the CLI... for as long as a real reshape
# takes"). By the time a script could plausibly `kill -9` the `shr-rs`
# process, it has normally already exited successfully on its own -- there
# is no live process actually "mid-expansion" to kill in the literal sense.
#
# The real, honest equivalent interruption -- the one this system is
# actually designed to survive, and the one a real crash/power-loss would
# produce -- is losing the live kernel reshape context entirely:
# `mdadm --stop` while the reshape is genuinely in progress (this also
# forcibly ends whatever userspace process might have been watching it,
# same practical effect as a kill), then `mdadm --assemble` back onto the
# SAME member devices. mdadm's own crash-recovery machinery (the
# `--backup-file` shr-rs's `execute_grow`/`prepare_backup_file` already
# wires up, D6) is what actually resumes the reshape from its last
# checkpoint -- this script proves THAT real mechanism, then proves
# `shr-rs reconcile` (via the `resize_pending` flag) picks the
# LVM/Btrfs layer back up once the resumed reshape finishes, which is
# exactly the sequence a real reboot-after-crash mid-reshape would need.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_POINT=/mnt/shr-smoke
TESTFILE="$MOUNT_POINT/resume-test-payload.bin"
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

echo "== SM-RESUME-1: arrange (a real 3-disk RAID5, grown to 4 disks by the"
echo "   expand under test; a 4th disk is added by that expand) =="
if ! fixture_up 16G 16G 16G 16G; then
    echo "SM-RESUME-1: BLOCKED (fixture_up failed)"
    exit 2
fi
DISKS="ata-LOOP_DISK_10,ata-LOOP_DISK_11,ata-LOOP_DISK_12"

CREATE_OUTPUT="$(sudo "$SHR_RS" --json create --mode shr --disks "$DISKS" --mount "$MOUNT_POINT" 2>&1)"
CREATE_EXIT=$?
echo "$CREATE_OUTPUT"
if [[ "$CREATE_EXIT" -ne 0 ]]; then
    echo "SM-RESUME-1: BLOCKED (create failed)"
    fixture_down >/dev/null 2>&1 || true
    exit 2
fi

MD_NAME="$(smoke_find_md_for_members loop10p1 loop11p1 loop12p1)"
if [[ -z "$MD_NAME" ]]; then
    echo "SM-RESUME-1: BLOCKED (could not find the real md device for band0's members)"
    exit 2
fi
echo "band0 is /dev/$MD_NAME"

if ! smoke_wait_sync_idle "$MD_NAME"; then
    echo "SM-RESUME-1: BLOCKED (initial resync never finished within the wait window)"
    exit 2
fi

echo "-- writing a known payload and recording its checksum BEFORE the reshape --"
sudo dd if=/dev/urandom of="$TESTFILE" bs=1M count=8 status=none || fail "could not write the test payload"
sudo sync
SHA_BEFORE="$(sudo sha256sum "$TESTFILE" | awk '{print $1}')"
echo "sha256 before: $SHA_BEFORE"

echo "== SM-RESUME-1: act, part 1 -- start a real reshape (3 -> 4 members) =="
# `--skip-scrub-check` is required, not a convenience: the fixture group was
# created moments ago, so the scrub-freshness gate ("no scrub successfully
# completed within the last 30 days") correctly refuses this expand and there
# would be no reshape to interrupt. Measured on the guest. This case is about
# resuming an interrupted reshape, not about the gate (SM-SCRUB-4 covers it).
EXPAND_OUTPUT="$(sudo "$SHR_RS" --json expand --add ata-LOOP_DISK_13 --skip-scrub-check 2>&1)"
EXPAND_EXIT=$?
echo "$EXPAND_OUTPUT"
if [[ "$EXPAND_EXIT" -ne 0 ]]; then
    echo "SM-RESUME-1: BLOCKED (arranging the reshape failed -- expand did not succeed)"
    exit 2
fi

if ! smoke_wait_sync_action "$MD_NAME" reshape 12 5; then
    echo "SM-RESUME-1: BLOCKED (sync_action never reached 'reshape' after expand)"
    exit 2
fi

echo "-- waiting for genuine reshape progress (not 0%, not finished) before interrupting --"
RESHAPE_PROGRESS_SEEN=0
for _ in $(seq 1 60); do
    # The "reshape = NN.N%" progress line is typically the THIRD line of an
    # md device's /proc/mdstat block (device list, then blocks/level/chunk,
    # then the progress bar) -- -A3, not -A1, or this misses it entirely.
    PROGRESS_LINE="$(grep -A3 "^${MD_NAME} :" /proc/mdstat | grep 'reshape =' || true)"
    if [[ -n "$PROGRESS_LINE" ]]; then
        PCT="$(echo "$PROGRESS_LINE" | grep -oE 'reshape *= *[0-9]+\.[0-9]+%' | grep -oE '[0-9]+\.[0-9]+' || echo 0)"
        echo "  reshape progress: ${PCT}% ($PROGRESS_LINE)"
        # awk float compare -- bash arithmetic can't handle the decimal.
        if awk -v p="$PCT" 'BEGIN{exit !(p > 1.0)}'; then
            RESHAPE_PROGRESS_SEEN=1
            break
        fi
    fi
    ACTION_NOW="$(cat "/sys/block/$MD_NAME/md/sync_action" 2>/dev/null || echo unknown)"
    [[ "$ACTION_NOW" == "idle" ]] && break  # finished before we ever caught it mid-way
    sleep 3
done
if [[ "$RESHAPE_PROGRESS_SEEN" -ne 1 ]]; then
    echo "SM-RESUME-1: BLOCKED (never observed real reshape progress > 1% before it either finished or the wait window ran out -- cannot honestly claim to have interrupted it 'mid-expansion')"
    exit 2
fi

echo "== SM-RESUME-1: act, part 2 -- interrupt (mdadm --stop, the honest"
echo "   equivalent of losing the live reshape context -- see header comment) =="
INTERRUPT_PROGRESS="$PCT"
sudo umount "$MOUNT_POINT" 2>/dev/null || true  # the array can't be stopped while its LV is mounted
if ! sudo mdadm --stop "/dev/$MD_NAME"; then
    echo "SM-RESUME-1: BLOCKED (mdadm --stop refused to stop the array mid-reshape -- cannot test interruption+resume this way on this mdadm version)"
    exit 2
fi
STOPPED_MDSTAT="$(cat /proc/mdstat)"
echo "$STOPPED_MDSTAT"
echo "$STOPPED_MDSTAT" | grep -q "^${MD_NAME} :" && fail "mdadm --stop did not actually stop $MD_NAME"

echo "== SM-RESUME-1: act, part 3 -- resume (mdadm --assemble onto the same members) =="
# --backup-file is passed defensively: shr-rs's own execute_grow/
# prepare_backup_file (D6) already created this file for the critical
# section at the START of the reshape, at this deterministic path. Recent
# mdadm/superblock-1.x versions track reshape position in the superblock
# itself and often don't need it again past the critical section, but
# passing it costs nothing if unneeded and can matter if assembly happens
# to need it.
BACKUP_FILE="/var/lib/shr-rs/backup-${MD_NAME}.bak"
if ! sudo mdadm --assemble "/dev/$MD_NAME" --backup-file="$BACKUP_FILE" /dev/loop10p1 /dev/loop11p1 /dev/loop12p1 /dev/loop13p1 2>&1; then
    echo "retrying assembly without --backup-file (it may not exist/be needed once past the reshape's initial critical section)..."
    if ! sudo mdadm --assemble "/dev/$MD_NAME" /dev/loop10p1 /dev/loop11p1 /dev/loop12p1 /dev/loop13p1; then
        echo "SM-RESUME-1: BLOCKED (mdadm --assemble could not reassemble the interrupted array)"
        exit 2
    fi
fi
sudo mdadm --run "/dev/$MD_NAME" 2>/dev/null || true
REASSEMBLED_MDSTAT="$(cat /proc/mdstat)"
echo "$REASSEMBLED_MDSTAT"
echo "$REASSEMBLED_MDSTAT" | grep -q "^${MD_NAME} :" || fail "$MD_NAME did not reappear after mdadm --assemble"

if ! smoke_wait_sync_action "$MD_NAME" reshape 6 3; then
    echo "note: sync_action did not show 'reshape' again within 18s of reassembly -- checking whether it simply finished very quickly instead of failing to resume"
fi

echo "-- waiting (bounded) for the resumed reshape to actually finish --"
if ! smoke_wait_sync_idle "$MD_NAME" 120 10; then
    fail "the resumed reshape never reached sync_action=idle within the wait window"
fi

echo "== SM-RESUME-1: act, part 4 -- reconcile the deferred LVM/Btrfs resize =="
RECONCILE_OUTPUT="$(sudo "$SHR_RS" --json reconcile 2>&1)"
RECONCILE_EXIT=$?
echo "$RECONCILE_OUTPUT"
[[ "$RECONCILE_EXIT" -eq 0 ]] || fail "shr-rs reconcile should have succeeded after the resumed reshape finished"
sudo mount "$MOUNT_POINT" 2>/dev/null || true
findmnt "$MOUNT_POINT" >/dev/null 2>&1 || sudo mount -a

echo "== SM-RESUME-1: assert (independent kernel/filesystem state observation) =="

FINAL_MDSTAT="$(cat /proc/mdstat)"
echo "$FINAL_MDSTAT"
FINAL_RAID_DISKS="$(cat "/sys/block/$MD_NAME/md/raid_disks" 2>/dev/null || echo 0)"
[[ "$FINAL_RAID_DISKS" == 4 ]] || fail "raid_disks is $FINAL_RAID_DISKS after resume+reconcile, expected exactly 4 (the target layout)"
FINAL_LEVEL="$(cat "/sys/block/$MD_NAME/md/level" 2>/dev/null || echo unknown)"
[[ "$FINAL_LEVEL" == "raid5" ]] || fail "level is '$FINAL_LEVEL' after resume+reconcile, expected 'raid5'"
FINAL_SYNC_ACTION="$(cat "/sys/block/$MD_NAME/md/sync_action" 2>/dev/null || echo unknown)"
[[ "$FINAL_SYNC_ACTION" == "idle" ]] || fail "sync_action is '$FINAL_SYNC_ACTION' after resume+reconcile, expected 'idle' (fully settled)"

echo "-- data integrity across the interruption, with page cache dropped so this"
echo "   really reads back through the (now 4-member) array, not a cached copy --"
FINDMNT="$(findmnt -no FSTYPE "$MOUNT_POINT" 2>/dev/null || true)"
if [[ "$FINDMNT" != "btrfs" ]]; then
    fail "$MOUNT_POINT is not mounted as btrfs after resume+reconcile (findmnt: $FINDMNT) -- cannot verify data"
else
    sudo sh -c 'echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null || true
    if [[ -f "$TESTFILE" ]]; then
        SHA_AFTER="$(sudo dd if="$TESTFILE" bs=1M count=8 iflag=direct status=none 2>/dev/null | sha256sum | awk '{print $1}')"
        if [[ -z "$SHA_AFTER" ]]; then
            SHA_AFTER="$(sudo sha256sum "$TESTFILE" | awk '{print $1}')"  # iflag=direct unsupported on this fs/kernel combo -- fall back to a normal read
        fi
        echo "sha256 after: $SHA_AFTER (before: $SHA_BEFORE)"
        [[ "$SHA_AFTER" == "$SHA_BEFORE" ]] || fail "test payload checksum changed across the interrupted-then-resumed reshape: before=$SHA_BEFORE after=$SHA_AFTER"
    else
        fail "test payload written before the reshape is missing after resume+reconcile"
    fi
fi

echo "(interrupted the reshape at ~${INTERRUPT_PROGRESS}% progress)"

echo "== SM-RESUME-1: cleanup + verify teardown (R4) =="
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-RESUME-1: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-RESUME-1: PASS"
else
    printf 'SM-RESUME-1: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
