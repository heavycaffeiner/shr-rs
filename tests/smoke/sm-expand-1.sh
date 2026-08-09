#!/usr/bin/env bash
# RAID1(2) -> RAID5(3) level promotion via real `shr-rs expand`,
# now that Step 8 has put real Btrfs on this guest so the full create()
# pipeline (needed as this test's starting state) can succeed at all.
#
# Judgment is by /proc/mdstat's level/raid_disks fields and observing a real
# reshape, never by shr-rs's own exit code, printed message, or
# `state.toml`'s `layout_version` (the design: judge by the
# change in /proc/mdstat's level and device count, not by layout_version).
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

echo "== SM-EXPAND-1: arrange (create a real 2-disk RAID1 array) =="
# All three loop devices are attached up front (simpler than a second
# "attach one more disk later" fixture helper) -- loop12 just sits unused
# until the expand step below.
if ! fixture_up 16G 16G 16G; then
    echo "SM-EXPAND-1: BLOCKED (fixture_up failed)"
    exit 2
fi

CREATE_OUTPUT="$(sudo "$SHR_RS" --json create --mode shr --disks ata-LOOP_DISK_10,ata-LOOP_DISK_11 --mount "$MOUNT_POINT" 2>&1)"
CREATE_EXIT=$?
echo "$CREATE_OUTPUT"
if [[ "$CREATE_EXIT" -ne 0 ]]; then
    echo "SM-EXPAND-1: BLOCKED (the RAID1 array this case expands could not be created)"
    exit 2
fi

# Real mdadm refuses a level-takeover reshape while the array's initial
# background resync (started by `create`) is still running --
# "mdadm: /dev/md0 is performing resync/recovery and cannot be taken over"
# -- a real constraint discovered by running this against real mdadm, not
# by inspection. Wait it out first; this is a timing property of the test
# fixture, not something `expand()` itself needs to preflight-check --
# shr-rs already handles the rejection correctly (F4's real-state
# verification correctly sees the array unchanged and rolls back the spare
# it had just attached), it just surfaces mdadm's own message rather than a
# friendlier "still resyncing" one. See the design
#
# Use the shared helper (90 attempts * 10s = 15 min) instead of an ad hoc
# 90*2s=3min /proc/mdstat text grep: every other smoke case that waits out
# an initial resync on a 16G member (sm-reboot-1.sh, sm-resume-1.sh, etc.)
# needed up to ~15 min on this TCG-emulated guest, so a 3-minute budget was
# very likely to time out and burn a full run on a false FAIL below rather
# than actually exercising the level-up reshape this case is for.
smoke_wait_sync_idle md0 || true

BEFORE_MDSTAT="$(cat /proc/mdstat)"
echo "-- before expand --"
echo "$BEFORE_MDSTAT"
echo "$BEFORE_MDSTAT" | grep -q "raid1" || fail "starting array is not raid1 -- got: $BEFORE_MDSTAT"
echo "$BEFORE_MDSTAT" | grep -q "resync" && fail "initial resync did not finish within the wait window -- cannot test a level-up reshape yet"
BEFORE_SIZE="$(cat /sys/block/md0/size 2>/dev/null || echo 0)"

echo "== SM-EXPAND-1: act (expand with a 3rd same-size disk) =="
# expand() refuses unless every band has a scrub that COMPLETED within
# the last 30 days (crates/shr-orchestrate/src/engine.rs scrub_staleness);
# a band this test just created has no scrub history at all, so without
# --skip-scrub-check this would always fail preflight with "has never been
# checked for errors" before ever reaching the reshape this case exists to
# test (same trap already documented for the TUI wizard).
EXPAND_OUTPUT="$(sudo "$SHR_RS" --json expand --add ata-LOOP_DISK_12 --skip-scrub-check 2>&1)"
EXPAND_EXIT=$?
echo "$EXPAND_OUTPUT"
echo "shr-rs expand exit code: $EXPAND_EXIT"

echo "== SM-EXPAND-1: assert (independent kernel state observation) =="

[[ "$EXPAND_EXIT" -eq 0 ]] || fail "expand should have succeeded (adding a 3rd same-size disk to a 2-disk RAID1)"

# The level/member-count change is immediate; don't wait for it here.
MDSTAT="$(cat /proc/mdstat)"
echo "-- after expand --"
echo "$MDSTAT"

echo "$MDSTAT" | grep -q "raid5" || fail "array did not promote to raid5 -- got: $MDSTAT"
MD0_LINE="$(echo "$MDSTAT" | grep '^md0' || true)"
MD0_MEMBER_COUNT="$(echo "$MD0_LINE" | grep -oE 'loop1[0-9]p[0-9]+\[[0-9]+\]' | wc -l)"
[[ "$MD0_MEMBER_COUNT" == 3 ]] || fail "md0 has $MD0_MEMBER_COUNT members after expand, expected exactly 3"
# The reshape progress line is a SEPARATE line from "md0 : active ..." in
# /proc/mdstat (not part of $MD0_LINE) -- check the whole block.
echo "$MDSTAT" | grep -q "reshape" || fail "no reshape observed in progress -- got: $MDSTAT"

# /sys/block/md0/size deliberately does NOT increase yet at this point: a
# level-takeover reshape only grows the array's reported size once its data
# movement finishes (real disks, real time -- not simulated), and shr-rs
# does not yet retry the LVM/Btrfs resize automatically once that happens
# (documented gap, the design -- no watcher exists). Asserting
# a size increase here would either be flatly wrong (the resize hasn't run)
# or force this smoke test to block for as long as a real reshape takes.
# The two things Phase 4 Step 8 actually needed to prove -- (1) a real
# level-up reshape starts and is recorded correctly, (2) `expand()` reports
# real success instead of misreporting the deferred resize as a total
# failure -- are both covered by the assertions above.
AFTER_SIZE="$(cat /sys/block/md0/size 2>/dev/null || echo 0)"
[[ "$AFTER_SIZE" == "$BEFORE_SIZE" ]] || fail "/sys/block/md0/size changed already (before=$BEFORE_SIZE after=$AFTER_SIZE) -- did the deferred-resize skip logic regress?"

echo "== SM-EXPAND-1: cleanup + verify teardown (R4) =="
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-EXPAND-1: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-EXPAND-1: PASS"
else
    printf 'SM-EXPAND-1: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
