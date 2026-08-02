#!/usr/bin/env bash
# SM-THROTTLE-1: `expand --priority` actually changes the kernel's reshape
# speed parameters, not just something in state.toml or a log line.
#
# `ThrottleController::apply_initial` (crates/shr-exec/src/throttle.rs)
# writes /proc/sys/dev/raid/speed_limit_max and speed_limit_min right after
# `mdadm --grow` succeeds, seeded from the chosen priority profile's
# `initial_speed_kb()` (Background=20000, Balanced=100000 (also the
# default), Max=500000 -- the RESHAPE_SPEED_CEILING_KB constant) and a fixed
# speed_limit_min=1000 (SPEED_LIMIT_MIN_DEFAULT_KB). Judgment here is a
# real, independent `cat` of those two /proc/sys files before and after a
# real `expand --priority background` -- Background's 20000 is far from any
# plausible kernel/system default, so an exact-value match after the call
# is strong, non-coincidental evidence the write actually happened (R1:
# judge by the observed change, not by the tool's own report).
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_POINT=/mnt/shr-smoke
EXPECTED_MAX_KB=20000  # ReshapePriority::Background.initial_speed_kb()
EXPECTED_MIN_KB=1000   # SPEED_LIMIT_MIN_DEFAULT_KB
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

BEFORE_MAX="$(sudo cat /proc/sys/dev/raid/speed_limit_max)"
BEFORE_MIN="$(sudo cat /proc/sys/dev/raid/speed_limit_min)"
echo "-- before expand: speed_limit_max=$BEFORE_MAX speed_limit_min=$BEFORE_MIN --"

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

AFTER_MAX="$(sudo cat /proc/sys/dev/raid/speed_limit_max)"
AFTER_MIN="$(sudo cat /proc/sys/dev/raid/speed_limit_min)"
echo "-- after expand: speed_limit_max=$AFTER_MAX speed_limit_min=$AFTER_MIN --"

# Why this is not `== $EXPECTED_MAX_KB`. Measured on the guest: after
# `--priority background` the kernel read 14000, not 20000 -- and that is the
# shipped design working, not a miss. `--priority` writes the Background
# profile's cap (20000), and then the adaptive throttle's first tick multiplies
# it by 0.7 (throttle.rs's `ThrottleDecision::Decrease(0.7)`), because
# `any_signal_unreadable` is true on loopback devices: they have no SMART
# identity, so `smartctl` cannot be read, and the engine deliberately treats an
# unreadable health signal as a reason to slow down rather than to assume
# health (see throttle.rs's own comment at the `disk_temp_max`/SMART fields --
# that posture is deliberate). 20000 * 0.7 = 14000 exactly.
#
# So the honest assertion is: the value must be the Background cap or the cap
# decayed by whole 0.7 ticks -- never some other profile's number, and never
# unchanged. Accepting any value <= 20000 would also pass if a different
# profile had been applied and then throttled hard, which is the thing this
# case exists to rule out.
matches_background_profile() {
    local observed="$1" candidate="$EXPECTED_MAX_KB"
    for _ in 0 1 2 3 4 5; do
        # bash has no floats; compare against the same round() the controller uses.
        [[ "$observed" == "$candidate" ]] && return 0
        candidate="$(awk -v c="$candidate" 'BEGIN { printf "%d", int(c * 0.7 + 0.5) }')"
    done
    return 1
}
matches_background_profile "$AFTER_MAX" || \
    fail "speed_limit_max is $AFTER_MAX after 'expand --priority background' -- expected the Background cap $EXPECTED_MAX_KB, or that cap decayed by whole adaptive-throttle 0.7 ticks (before was $BEFORE_MAX)"
[[ "$AFTER_MIN" == "$EXPECTED_MIN_KB" ]] || \
    fail "speed_limit_min is $AFTER_MIN after 'expand --priority background', expected exactly $EXPECTED_MIN_KB (before was $BEFORE_MIN)"
[[ "$AFTER_MAX" != "$BEFORE_MAX" || "$BEFORE_MAX" == "$EXPECTED_MAX_KB" ]] || \
    fail "speed_limit_max did not change at all (before=$BEFORE_MAX after=$AFTER_MAX) -- expected --priority to actually write the kernel parameter"

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
