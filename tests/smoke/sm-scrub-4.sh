#!/usr/bin/env bash
# (negative, both directions): scrub and expand must never run at
# the same time on the same group.
#
# Direction A (expand blocked while a scrub is running) already has real
# evidence behind it -- an earlier smoke run already exercised it
# via `preview_expand`. This script re-proves it directly against the real
# CLI anyway (cheap, and it's the natural first half of a two-direction
# test), but its actual NEW contribution is direction B: a scrub start
# refused while a REAL reshape is in progress. That direction was never
# actually run before -- `expand()`'s "band has background activity in
# progress" guard covers it in the engine, per its own doc comment, but
# nothing had exercised it end-to-end against a real mdadm reshape.
#
# Judgment for both directions is the same shape: read
# /sys/block/<md>/md/sync_action independently before and after the refused
# call, and confirm it's exactly what it was before the refused command ran
# (never shr-rs's exit code or message alone).
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

echo "== SM-SCRUB-4: arrange (create a real 3-disk RAID5; a 4th disk is held"
echo "   back for the expand attempts in both directions) =="
if ! fixture_up 16G 16G 16G 16G; then
    echo "SM-SCRUB-4: BLOCKED (fixture_up failed)"
    exit 2
fi
DISKS="ata-LOOP_DISK_10,ata-LOOP_DISK_11,ata-LOOP_DISK_12"

CREATE_OUTPUT="$(sudo "$SHR_RS" --json create --mode shr --disks "$DISKS" --mount "$MOUNT_POINT" 2>&1)"
CREATE_EXIT=$?
echo "$CREATE_OUTPUT"
if [[ "$CREATE_EXIT" -ne 0 ]]; then
    echo "SM-SCRUB-4: BLOCKED (create failed)"
    fixture_down >/dev/null 2>&1 || true
    exit 2
fi

MD_NAME="$(smoke_find_md_for_members loop10p1 loop11p1 loop12p1)"
if [[ -z "$MD_NAME" ]]; then
    echo "SM-SCRUB-4: BLOCKED (could not find the real md device for band0's members)"
    exit 2
fi
echo "band0 is /dev/$MD_NAME"

if ! smoke_wait_sync_idle "$MD_NAME"; then
    echo "SM-SCRUB-4: BLOCKED (initial resync never finished within the wait window)"
    exit 2
fi

echo ""
echo "== SM-SCRUB-4 direction A: expand refused while a scrub is running =="

SCRUB_START_OUTPUT="$(sudo "$SHR_RS" --json fs scrub start 2>&1)"
SCRUB_START_EXIT=$?
echo "$SCRUB_START_OUTPUT"
if [[ "$SCRUB_START_EXIT" -ne 0 ]]; then
    echo "SM-SCRUB-4: BLOCKED (arranging direction A failed -- scrub start itself did not succeed)"
    exit 2
fi

if ! smoke_wait_sync_action "$MD_NAME" check 12 5; then
    echo "SM-SCRUB-4: BLOCKED (sync_action never reached 'check' after scrub start)"
    exit 2
fi
BEFORE_A_RAID_DISKS="$(cat "/sys/block/$MD_NAME/md/raid_disks" 2>/dev/null || echo unknown)"
echo "raid_disks before the refused expand: $BEFORE_A_RAID_DISKS"

EXPAND_DURING_SCRUB_OUTPUT="$(sudo "$SHR_RS" --json expand --add ata-LOOP_DISK_13 2>&1)"
EXPAND_DURING_SCRUB_EXIT=$?
echo "$EXPAND_DURING_SCRUB_OUTPUT"
echo "expand-during-scrub exit code: $EXPAND_DURING_SCRUB_EXIT"

[[ "$EXPAND_DURING_SCRUB_EXIT" -ne 0 ]] || fail "direction A: expand should have been refused while a scrub is running (exit 0 instead)"

# Refused is not enough -- it must be refused for THIS reason. The freshness
# gate (no recent scrub in the history) would also refuse this same command,
# and this check exists
# precisely because the tool used to report that stale-history message while a
# scrub was visibly running, telling the operator to do the thing they were
# already doing. Asserting only the exit code cannot tell the two apart.
echo "$EXPAND_DURING_SCRUB_OUTPUT" | grep -qi 'background activity' || \
    fail "direction A: expand was refused, but not for the in-progress reason -- got: $EXPAND_DURING_SCRUB_OUTPUT"

AFTER_A_SYNC_ACTION="$(cat "/sys/block/$MD_NAME/md/sync_action" 2>/dev/null || echo unknown)"
AFTER_A_RAID_DISKS="$(cat "/sys/block/$MD_NAME/md/raid_disks" 2>/dev/null || echo unknown)"
echo "sync_action after the refused expand: $AFTER_A_SYNC_ACTION, raid_disks: $AFTER_A_RAID_DISKS"
[[ "$AFTER_A_SYNC_ACTION" == "check" ]] || \
    fail "direction A: sync_action is '$AFTER_A_SYNC_ACTION' after the refused expand, expected it to still be 'check' (the scrub, undisturbed) -- a 'reshape' here would mean the expand actually started"
[[ "$AFTER_A_RAID_DISKS" == "$BEFORE_A_RAID_DISKS" ]] || \
    fail "direction A: raid_disks changed across the refused expand: before=$BEFORE_A_RAID_DISKS after=$AFTER_A_RAID_DISKS"

CANCEL_OUTPUT="$(sudo "$SHR_RS" --json fs scrub cancel 2>&1)"
CANCEL_EXIT=$?
echo "$CANCEL_OUTPUT"
if [[ "$CANCEL_EXIT" -ne 0 ]]; then
    echo "SM-SCRUB-4: BLOCKED (could not cancel the direction-A scrub to reset for direction B)"
    exit 2
fi
if ! smoke_wait_sync_idle "$MD_NAME" 12 5; then
    echo "SM-SCRUB-4: BLOCKED (sync_action never returned to 'idle' after scrub cancel -- cannot reset for direction B)"
    exit 2
fi

echo ""
echo "== SM-SCRUB-4 direction B (the previously-unverified direction): scrub"
echo "   refused while a REAL reshape is in progress =="

# `--skip-scrub-check` is required here: direction A CANCELLED its scrub
# rather than letting it finish, so no successful scrub was ever recorded and
# the freshness gate ("no scrub successfully completed within the last 30
# days") refuses this expand. Measured on the guest -- without the flag this
# step exits 1 and direction B never gets a reshape to test against. That gate
# is not what direction B is measuring; the reshape-vs-scrub interlock is.
EXPAND_OUTPUT="$(sudo "$SHR_RS" --json expand --add ata-LOOP_DISK_13 --skip-scrub-check 2>&1)"
EXPAND_EXIT=$?
echo "$EXPAND_OUTPUT"
if [[ "$EXPAND_EXIT" -ne 0 ]]; then
    echo "SM-SCRUB-4: BLOCKED (arranging direction B failed -- the real expand itself did not succeed)"
    exit 2
fi

if ! smoke_wait_sync_action "$MD_NAME" reshape 12 5; then
    echo "SM-SCRUB-4: BLOCKED (sync_action never reached 'reshape' after a real expand -- cannot test direction B)"
    exit 2
fi
BEFORE_B_RAID_DISKS="$(cat "/sys/block/$MD_NAME/md/raid_disks" 2>/dev/null || echo unknown)"
echo "raid_disks while reshaping, before the refused scrub: $BEFORE_B_RAID_DISKS"

SCRUB_DURING_EXPAND_OUTPUT="$(sudo "$SHR_RS" --json fs scrub start 2>&1)"
SCRUB_DURING_EXPAND_EXIT=$?
echo "$SCRUB_DURING_EXPAND_OUTPUT"
echo "scrub-during-expand exit code: $SCRUB_DURING_EXPAND_EXIT"

[[ "$SCRUB_DURING_EXPAND_EXIT" -ne 0 ]] || fail "direction B: scrub start should have been refused while a reshape is in progress (exit 0 instead)"

AFTER_B_SYNC_ACTION="$(cat "/sys/block/$MD_NAME/md/sync_action" 2>/dev/null || echo unknown)"
AFTER_B_RAID_DISKS="$(cat "/sys/block/$MD_NAME/md/raid_disks" 2>/dev/null || echo unknown)"
echo "sync_action after the refused scrub: $AFTER_B_SYNC_ACTION, raid_disks: $AFTER_B_RAID_DISKS"
[[ "$AFTER_B_SYNC_ACTION" == "reshape" ]] || \
    fail "direction B: sync_action is '$AFTER_B_SYNC_ACTION' after the refused scrub, expected it to still be 'reshape' (the expand, undisturbed) -- a 'check' here would mean the scrub actually started"
[[ "$AFTER_B_RAID_DISKS" == "$BEFORE_B_RAID_DISKS" ]] || \
    fail "direction B: raid_disks changed across the refused scrub: before=$BEFORE_B_RAID_DISKS after=$AFTER_B_RAID_DISKS"

echo "== SM-SCRUB-4: cleanup + verify teardown (R4) =="
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-SCRUB-4: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-SCRUB-4: PASS"
else
    printf 'SM-SCRUB-4: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
