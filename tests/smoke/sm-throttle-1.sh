#!/usr/bin/env bash
# SM-THROTTLE-1: `expand --priority` actually changes the kernel's sync speed
# parameters for that band, not just something in state.toml or a log line.
#
# `ThrottleController::apply_initial` (crates/shr-exec/src/throttle.rs)
# writes the band's own `/sys/block/<md>/md/sync_speed_{min,max}` right after
# `mdadm --grow` succeeds, both of them, derived from the chosen profile:
# `Background` claims 0.35 of what the array has been measured able to do and
# floors at 0.20 of it, and until an estimate exists it uses the bootstrap
# pair (60000 / 25000 KB/s).
#
# BOTH limits matter, and that is the point of this case. Writing only a
# ceiling is what made `--priority max` slower than it asked for: the kernel
# reduces the sync rate toward `sync_speed_min` whenever non-sync IO touches
# the members, and on a live NAS there always is some, so a 1 MB/s floor
# governed every operation regardless of the ceiling above it.
#
# Judgment is a real, independent `cat` of those two files before and after a
# real `expand --priority background` (R1: judge by the observed change, not
# by the tool's own report).
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_POINT=/mnt/shr-smoke
# SyncPriority::Background.limits(None) -- the bootstrap pair, used until
# this band has a measured capability estimate.
EXPECTED_MAX_KB=60000
EXPECTED_MIN_KB=25000
# throttle.rs's STREAM_FLOOR_ABS_KB: below this md stutters rather than
# streams, so no profile's floor is ever allowed under it.
STREAM_FLOOR_KB=15000
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

echo "== SM-THROTTLE-1: arrange (a real 2-disk RAID1, promoted to RAID5 by the"
echo "   expand under test -- same shape as sm-expand-1.sh) =="
if ! fixture_up 16G 16G 16G; then
    echo "SM-THROTTLE-1: BLOCKED (fixture_up failed)"
    exit 2
fi

CREATE_OUTPUT="$(sudo "$SHR_RS" --json create --mode shr --disks ata-LOOP_DISK_10,ata-LOOP_DISK_11 --mount "$MOUNT_POINT" 2>&1)"
CREATE_EXIT=$?
echo "$CREATE_OUTPUT"
if [[ "$CREATE_EXIT" -ne 0 ]]; then
    echo "SM-THROTTLE-1: BLOCKED (create failed)"
    fixture_down >/dev/null 2>&1 || true
    exit 2
fi

MD_NAME="$(smoke_find_md_for_members loop10p1 loop11p1)"
if [[ -z "$MD_NAME" ]]; then
    echo "SM-THROTTLE-1: BLOCKED (could not find the real md device for the new array)"
    exit 2
fi

if ! smoke_wait_sync_idle "$MD_NAME"; then
    echo "SM-THROTTLE-1: BLOCKED (initial resync never finished within the wait window -- a level-up reshape cannot start yet, see sm-expand-1.sh's own note on this mdadm constraint)"
    exit 2
fi

BEFORE_MAX="$(smoke_sync_limit "$MD_NAME" max)"
BEFORE_MIN="$(smoke_sync_limit "$MD_NAME" min)"
BEFORE_ORIGIN="$(smoke_sync_limit_origin "$MD_NAME" max)"
echo "-- before expand: max=$BEFORE_MAX min=$BEFORE_MIN origin=$BEFORE_ORIGIN --"

echo "== SM-THROTTLE-1: act (expand --priority background) =="
# `--skip-scrub-check` is required, not a convenience: the fixture group was
# created seconds ago, so the pre-reshape check ("no scrub successfully
# completed within the last 30 days") correctly refuses the expand. Measured
# on the guest without this flag -- expand exited 1 and `--priority` never ran,
# which then made the two speed_limit assertions below fail for a reason that
# has nothing to do with throttling. its own gate is SM-SCRUB-4's subject;
# this case is about whether `--priority` actually writes the kernel
# parameter, so it takes the documented escape hatch rather than spending a
# full scrub cycle to reach the same starting line.
EXPAND_OUTPUT="$(sudo "$SHR_RS" --json expand --add ata-LOOP_DISK_12 --priority background --skip-scrub-check 2>&1)"
EXPAND_EXIT=$?
echo "$EXPAND_OUTPUT"
echo "expand exit code: $EXPAND_EXIT"
[[ "$EXPAND_EXIT" -eq 0 ]] || fail "expand --priority background should have succeeded"

echo "== SM-THROTTLE-1: assert (independent kernel state observation) =="

MDSTAT="$(cat /proc/mdstat)"
echo "$MDSTAT"
echo "$MDSTAT" | grep -q "raid5" || fail "array did not promote to raid5 -- expand may not have actually run"

AFTER_MAX="$(smoke_sync_limit "$MD_NAME" max)"
AFTER_MIN="$(smoke_sync_limit "$MD_NAME" min)"
AFTER_ORIGIN="$(smoke_sync_limit_origin "$MD_NAME" max)"
echo "-- after expand: max=$AFTER_MAX min=$AFTER_MIN origin=$AFTER_ORIGIN --"

# Why this is not `== $EXPECTED_MAX_KB`. `--priority` writes the Background
# profile's ceiling, and then the throttle's first tick may already have
# scaled it by 0.7: loopback devices have no SMART identity, so `smartctl`
# cannot be read, and an unreadable safety signal is deliberately a reason to
# slow down rather than to assume health. So the honest assertion is: the
# ceiling must be the profile's own or that ceiling decayed by whole 0.7
# ticks -- never some other profile's number, and never unchanged. Accepting
# anything <= 60000 would also pass if a different profile had been applied
# and then throttled hard, which is what this case exists to rule out.
matches_background_profile() {
    local observed="$1" candidate="$EXPECTED_MAX_KB"
    for _ in 0 1 2 3 4 5; do
        # bash has no floats; compare against the same round() the controller uses.
        [[ "$observed" == "$candidate" ]] && return 0
        candidate="$(awk -v c="$candidate" 'BEGIN { printf "%d", int(c * 0.7 + 0.5) }')"
        # A decrease is floored at the profile's own minimum, never below it.
        (( candidate < EXPECTED_MIN_KB )) && candidate="$EXPECTED_MIN_KB"
    done
    return 1
}
matches_background_profile "$AFTER_MAX" || \
    fail "sync_speed_max is $AFTER_MAX after 'expand --priority background' -- expected the Background ceiling $EXPECTED_MAX_KB, or that ceiling decayed by whole adaptive-throttle 0.7 ticks and floored at $EXPECTED_MIN_KB (before was $BEFORE_MAX)"
[[ "$AFTER_MIN" == "$EXPECTED_MIN_KB" ]] || \
    fail "sync_speed_min is $AFTER_MIN after 'expand --priority background', expected exactly the profile's own floor $EXPECTED_MIN_KB (before was $BEFORE_MIN)"
# The defect this replaced: the floor was written as a fixed 1000 KB/s for
# every profile, and the kernel pulls the sync rate toward it under any
# non-sync IO.
(( AFTER_MIN >= STREAM_FLOOR_KB )) || \
    fail "sync_speed_min is $AFTER_MIN, below the $STREAM_FLOOR_KB streaming bound -- at that floor md bursts and backs off instead of streaming, and both the sync and everyday IO pay the seek"
[[ "$AFTER_MAX" != "$BEFORE_MAX" || "$BEFORE_MAX" == "$EXPECTED_MAX_KB" ]] || \
    fail "sync_speed_max did not change at all (before=$BEFORE_MAX after=$AFTER_MAX) -- expected --priority to actually write the kernel parameter"
if [[ "$AFTER_ORIGIN" != absent ]]; then
    [[ "$AFTER_ORIGIN" == local ]] || \
        fail "this kernel HAS per-array limits but $MD_NAME's ceiling still reads '$AFTER_ORIGIN' -- the write went host-wide, which cannot express a per-band profile"
fi

echo "== SM-THROTTLE-1: cleanup + verify teardown (R4) =="
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-THROTTLE-1: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-THROTTLE-1: PASS"
else
    printf 'SM-THROTTLE-1: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
