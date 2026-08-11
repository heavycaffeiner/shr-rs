#!/usr/bin/env bash
# SM-THROTTLE-3: a band's speed limits are BORROWED, not taken.
#
# Every limit shr-rs writes now goes to the band's own
# `/sys/block/<md>/md/sync_speed_{min,max}`, which shadow the host-wide
# `/proc/sys/dev/raid/speed_limit_*` for that array alone. Two things follow,
# and this case judges both against an independent `cat` of the real kernel
# files rather than shr-rs's own report (R1):
#
#   1. The operator's own host-wide setting is untouched throughout. Before
#      this, `expand --priority background` left the host-wide cap at
#      20000 KB/s (lower still once the adaptive throttle had decayed it) for
#      every array on the machine until somebody rebooted, which then
#      throttled whatever read it next.
#   2. Once the work is over, the band's limits are handed back exactly: a
#      write of the literal `system`, which clears the local value, not a
#      restore of a remembered number. A floor left behind silently governs
#      every later operation on that array.
#
# Step 2 is the one that could not be tested anywhere else: the clear is
# driven by `shr-rs internal reshape-throttle-tick`, a separate process fired
# by a systemd timer, working purely from what is on disk.
#
# On a kernel with no per-array attributes shr-rs falls back to the host-wide
# pair and to `state.toml`'s `saved_speed_limit_max_kb` save-and-restore.
# That path is detected and reported as SKIPPED here rather than asserted
# against, because this guest has the attributes and cannot exercise it.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_POINT=/mnt/shr-smoke
STATE=/var/lib/shr-rs/state.toml
SPEED_MAX=/proc/sys/dev/raid/speed_limit_max
# A deliberately odd number, nowhere near any profile's own value and nowhere
# near the kernel default 200000. If this exact figure is still there at the
# end, nothing shr-rs did went host-wide.
SENTINEL_KB=137000
RESULT=PASS
FAILURES=()
SKIPPED=()

fail() {
    RESULT=FAIL
    FAILURES+=("$1")
    echo "FAIL: $1" >&2
}

skip() {
    SKIPPED+=("$1")
    echo "SKIPPED: $1" >&2
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

PER_ARRAY="$(smoke_sync_limit_origin "$MD_NAME" max)"
if [[ "$PER_ARRAY" == absent ]]; then
    skip "this kernel has no /sys/block/$MD_NAME/md/sync_speed_max, so shr-rs takes the host-wide fallback path and there is nothing per-array to borrow or hand back. The fallback's own save-and-restore is covered by state.toml's saved_speed_limit_max_kb, which this case cannot exercise here."
    echo "SM-THROTTLE-3: SKIPPED (no per-array limit attributes on this kernel)"
    exit 0
fi

# A scrub is refused while anything else is running, so the initial resync
# has to finish first -- same constraint sm-scrub-3.sh documents.
if ! smoke_wait_sync_idle "$MD_NAME"; then
    echo "SM-THROTTLE-3: BLOCKED (initial resync never finished within the wait window)"
    exit 2
fi

# Establish the operator's "own" host-wide setting AFTER create, so nothing
# in the arrange phase can be what puts it there.
echo "$SENTINEL_KB" | sudo tee "$SPEED_MAX" >/dev/null
BEFORE_MAX="$(cat "$SPEED_MAX")"
echo "-- operator's host-wide speed_limit_max before the scrub: $BEFORE_MAX --"
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

echo "== SM-THROTTLE-3: assert 1 (the band's own limits moved, the host's did not) =="
DURING_MAX="$(smoke_sync_limit "$MD_NAME" max)"
DURING_ORIGIN="$(smoke_sync_limit_origin "$MD_NAME" max)"
HOST_DURING="$(cat "$SPEED_MAX")"
echo "band sync_speed_max during the scrub: $DURING_MAX ($DURING_ORIGIN); host-wide: $HOST_DURING"
[[ "$DURING_ORIGIN" == local ]] || \
    fail "the band's ceiling reads '$DURING_ORIGIN' during its own scrub -- shr-rs wrote somewhere else"
[[ "$HOST_DURING" == "$SENTINEL_KB" ]] || \
    fail "the host-wide speed_limit_max is $HOST_DURING, expected the operator's own $SENTINEL_KB untouched -- a per-band profile must not reach every other array on the machine"

# The scrub itself has to have really started -- a test that only proved
# the kernel parameter moved would pass just as happily if `--priority`
# wrote the ceiling and then failed to start any scrub at all.
SYNC_ACTION="$(cat "/sys/block/$MD_NAME/md/sync_action" 2>/dev/null || echo unknown)"
echo "sync_action after scrub start: $SYNC_ACTION"
[[ "$SYNC_ACTION" == "check" ]] || \
    fail "sync_action is '$SYNC_ACTION' right after 'fs scrub start', expected 'check'"

echo "== SM-THROTTLE-3: assert 2 (nothing host-wide was borrowed at all) =="
SAVED_LINE="$(sudo grep -E '^saved_speed_limit_max_kb' "$STATE" 2>/dev/null || true)"
echo "state.toml: ${SAVED_LINE:-<absent>}"
[[ -z "$SAVED_LINE" ]] || \
    fail "state.toml recorded a host-wide value to restore (${SAVED_LINE}) even though this kernel has per-array limits -- the fallback path ran when it should not have"

echo "== SM-THROTTLE-3: act 2 (let the scrub end, then run one throttle tick) =="
# Cancelling is a legitimate end to a scrub and takes seconds instead of
# minutes; what step 2 is about is what happens once no band is busy, not
# how the work finished. `scrub cancel` writes `idle` to the same
# sync_action file the kernel would have reached on its own.
sudo "$SHR_RS" --json fs scrub cancel >/dev/null 2>&1 || true
if ! smoke_wait_sync_idle "$MD_NAME" 30 2; then
    echo "SM-THROTTLE-3: BLOCKED (the scrub never went back to idle, so there is nothing to hand back yet)"
    exit 2
fi

# This is the systemd timer's entrypoint, run as its own process exactly
# the way `shr-rs-throttle-tick.timer` invokes it -- it has no memory of
# the scrub above and must work purely from what is on disk.
TICK_OUTPUT="$(sudo "$SHR_RS" --json internal reshape-throttle-tick 2>&1)"
echo "$TICK_OUTPUT"

echo "== SM-THROTTLE-3: assert 3 (the band's limits were handed back) =="
for which in min max; do
    ORIGIN="$(smoke_sync_limit_origin "$MD_NAME" "$which")"
    VALUE="$(smoke_sync_limit "$MD_NAME" "$which")"
    echo "sync_speed_$which after the tick: $VALUE ($ORIGIN)"
    [[ "$ORIGIN" == system ]] || \
        fail "sync_speed_$which still reads '$ORIGIN' after the work finished -- a limit left behind silently governs every later operation on this array"
done

AFTER_HOST="$(cat "$SPEED_MAX")"
[[ "$AFTER_HOST" == "$SENTINEL_KB" ]] || \
    fail "the host-wide speed_limit_max is $AFTER_HOST, expected the operator's own $SENTINEL_KB still untouched"

PRIORITY_LEFT="$(sudo grep -cE '^sync_priority' "$STATE" 2>/dev/null || true)"
[[ "$PRIORITY_LEFT" == "0" ]] || \
    fail "state.toml still records a sync_priority for a band with nothing running; that stale profile would govern this band's NEXT operation"

echo "== SM-THROTTLE-3: cleanup + verify teardown (R4) =="
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-THROTTLE-3: verdict =="
if [[ "${#SKIPPED[@]}" -gt 0 ]]; then
    printf '  skipped: %s\n' "${SKIPPED[@]}"
fi
if [[ "$RESULT" == PASS ]]; then
    echo "SM-THROTTLE-3: PASS"
else
    printf 'SM-THROTTLE-3: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
