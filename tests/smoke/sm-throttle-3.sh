#!/usr/bin/env bash
# SM-THROTTLE-3: the host-wide RAID speed limit is BORROWED, not taken.
#
# `/proc/sys/dev/raid/speed_limit_max` is one setting for the whole host,
# and shr-rs writes it from two places: the reshape throttle
# (`expand --priority`) and, now, `fs scrub start --priority`. Until this
# was fixed nothing ever put it back, so an `expand --priority background`
# left the cap at 20000 KB/s (or lower, once the adaptive throttle had
# decayed it) for every array on the machine until somebody rebooted --
# which then throttled whatever read it next, most visibly a scrub, for a
# reason nothing in shr-rs ever reported.
#
# Three facts, each judged by an INDEPENDENT `cat` of the real kernel file
# or a real read of state.toml, never by shr-rs's own exit code or report
# (R1: judge by the observed change):
#
#   1. `fs scrub start --priority max` writes the profile's ceiling
#      (RESHAPE_SPEED_CEILING_KB = 500000) and the mdadm `check` really
#      starts.
#   2. The value that was there BEFORE is recorded in state.toml
#      (`saved_speed_limit_max_kb`), which is the only thing that makes
#      step 3 possible from a different process minutes later.
#   3. Once the scrub is over, a throttle tick puts the operator's own
#      number back and clears the slot.
#
# Step 3 is the one that could not be tested anywhere else: the restore is
# driven by `shr-rs internal reshape-throttle-tick`, a separate process
# fired by a systemd timer, reading a value this process wrote to disk.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_POINT=/mnt/shr-smoke
STATE=/var/lib/shr-rs/state.toml
SPEED_MAX=/proc/sys/dev/raid/speed_limit_max
# A deliberately odd number, nowhere near any profile's own value
# (Background 20000, Balanced 100000, Max 500000) and nowhere near the
# kernel default 200000. If this exact figure comes back at the end, it
# came back from the saved slot and from nowhere else.
SENTINEL_KB=137000
EXPECTED_CEILING_KB=500000  # ReshapePriority::Max.initial_speed_kb()
RESULT=PASS
FAILURES=()

fail() {
    RESULT=FAIL
    FAILURES+=("$1")
    echo "FAIL: $1" >&2
}

# The host's own pre-test value, put back on the way out no matter how this
# exits -- this test writes a machine-wide kernel setting, so leaving it
# wherever the run happened to end would be a side effect on the host, not
# a test result.
ORIGINAL_MAX="$(cat "$SPEED_MAX")"
cleanup() {
    sudo umount "$MOUNT_POINT" 2>/dev/null || true
    fixture_down >/dev/null 2>&1 || true
    echo "$ORIGINAL_MAX" | sudo tee "$SPEED_MAX" >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== SM-THROTTLE-3: arrange (a real 3-disk RAID5 over the loop band) =="
# 8G members, not the 16G its sibling cases use. Nothing here depends on
# capacity -- what is under test is a kernel parameter -- while everything
# here waits twice on real array activity (the initial resync, then the
# scrub reaching idle), and both scale with member size. Smaller images also
# fit a guest with no separate smoke disk.
#
# 8G is the FLOOR, not a free choice: the planner aligns bands to
# DEFAULT_BAND_ALIGNMENT (4 GiB) after reserving 128 MiB of head and 8 MiB
# of tail, so a 4G member has 3.87 GiB of usable extent, which rounds down
# to zero whole bands. Measured on the guest: `create` refuses with
# "no redundant band could be formed from the given disks".
if ! fixture_up 8G 8G 8G; then
    echo "SM-THROTTLE-3: BLOCKED (fixture_up failed)"
    exit 2
fi

CREATE_OUTPUT="$(sudo "$SHR_RS" --json create --mode shr \
    --disks ata-LOOP_DISK_10,ata-LOOP_DISK_11,ata-LOOP_DISK_12 --mount "$MOUNT_POINT" 2>&1)"
CREATE_EXIT=$?
echo "$CREATE_OUTPUT"
if [[ "$CREATE_EXIT" -ne 0 ]]; then
    echo "SM-THROTTLE-3: BLOCKED (create failed)"
    exit 2
fi

MD_NAME="$(smoke_find_md_for_members loop10p1 loop11p1 loop12p1)"
if [[ -z "$MD_NAME" ]]; then
    echo "SM-THROTTLE-3: BLOCKED (could not find the real md device for band0)"
    exit 2
fi
echo "band0 is /dev/$MD_NAME"

# A scrub is refused while anything else is running, so the initial resync
# has to finish first -- same constraint sm-scrub-3.sh documents.
if ! smoke_wait_sync_idle "$MD_NAME"; then
    echo "SM-THROTTLE-3: BLOCKED (initial resync never finished within the wait window)"
    exit 2
fi

# Establish the operator's "own" setting AFTER create, so nothing in the
# arrange phase can be what puts it there.
echo "$SENTINEL_KB" | sudo tee "$SPEED_MAX" >/dev/null
BEFORE_MAX="$(cat "$SPEED_MAX")"
echo "-- operator's speed_limit_max before the scrub: $BEFORE_MAX --"
if [[ "$BEFORE_MAX" != "$SENTINEL_KB" ]]; then
    echo "SM-THROTTLE-3: BLOCKED (could not set the sentinel value; got $BEFORE_MAX)"
    exit 2
fi

echo "== SM-THROTTLE-3: act 1 (fs scrub start --priority max) =="
SCRUB_OUTPUT="$(sudo "$SHR_RS" --json fs scrub start --priority max 2>&1)"
SCRUB_EXIT=$?
echo "$SCRUB_OUTPUT"
echo "exit code: $SCRUB_EXIT"
[[ "$SCRUB_EXIT" -eq 0 ]] || fail "fs scrub start --priority max should have succeeded"

echo "== SM-THROTTLE-3: assert 1 (the ceiling reached the kernel) =="
DURING_MAX="$(cat "$SPEED_MAX")"
echo "speed_limit_max during the scrub: $DURING_MAX"
[[ "$DURING_MAX" == "$EXPECTED_CEILING_KB" ]] || \
    fail "speed_limit_max is $DURING_MAX during a --priority max scrub, expected exactly \
$EXPECTED_CEILING_KB (was $BEFORE_MAX before)"

# The scrub itself has to have really started -- a test that only proved
# the kernel parameter moved would pass just as happily if `--priority`
# wrote the ceiling and then failed to start any scrub at all.
SYNC_ACTION="$(cat "/sys/block/$MD_NAME/md/sync_action" 2>/dev/null || echo unknown)"
echo "sync_action after scrub start: $SYNC_ACTION"
[[ "$SYNC_ACTION" == "check" ]] || \
    fail "sync_action is '$SYNC_ACTION' right after 'fs scrub start', expected 'check'"

echo "== SM-THROTTLE-3: assert 2 (the operator's value was recorded, not discarded) =="
SAVED_LINE="$(sudo grep -E '^saved_speed_limit_max_kb' "$STATE" 2>/dev/null || true)"
echo "state.toml: ${SAVED_LINE:-<absent>}"
[[ "$SAVED_LINE" == *"$SENTINEL_KB"* ]] || \
    fail "state.toml does not record the pre-scrub speed_limit_max ($SENTINEL_KB); without it \
nothing can ever put the operator's own value back (line was: ${SAVED_LINE:-<absent>})"

echo "== SM-THROTTLE-3: act 2 (let the scrub end, then run one throttle tick) =="
# Cancelling is a legitimate end to a scrub and takes seconds instead of
# minutes; what step 3 is about is what happens once no band is busy, not
# how the work finished. `scrub cancel` writes `idle` to the same
# sync_action file the kernel would have reached on its own.
sudo "$SHR_RS" --json fs scrub cancel >/dev/null 2>&1 || true
if ! smoke_wait_sync_idle "$MD_NAME" 30 2; then
    echo "SM-THROTTLE-3: BLOCKED (the scrub never went back to idle, so there is nothing to restore yet)"
    exit 2
fi

# This is the systemd timer's entrypoint, run as its own process exactly
# the way `shr-rs-throttle-tick.timer` invokes it -- it has no memory of
# the scrub above and must work purely from what is on disk.
TICK_OUTPUT="$(sudo "$SHR_RS" --json internal reshape-throttle-tick 2>&1)"
echo "$TICK_OUTPUT"

echo "== SM-THROTTLE-3: assert 3 (the borrowed value was handed back) =="
AFTER_MAX="$(cat "$SPEED_MAX")"
echo "speed_limit_max after the tick: $AFTER_MAX"
[[ "$AFTER_MAX" == "$SENTINEL_KB" ]] || \
    fail "speed_limit_max is $AFTER_MAX after the work finished, expected the operator's own \
$SENTINEL_KB back (it was $EXPECTED_CEILING_KB during the scrub)"

SAVED_AFTER="$(sudo grep -cE '^saved_speed_limit_max_kb' "$STATE" 2>/dev/null || true)"
[[ "$SAVED_AFTER" == "0" ]] || \
    fail "state.toml still carries saved_speed_limit_max_kb after the restore; every later tick \
would keep rewriting the same value over whatever the operator set next"

echo "== SM-THROTTLE-3: cleanup + verify teardown (R4) =="
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-THROTTLE-3: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-THROTTLE-3: PASS"
else
    printf 'SM-THROTTLE-3: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
