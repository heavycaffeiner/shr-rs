#!/usr/bin/env bash
# SM-THROTTLE-5: the throttle is not a one-way ratchet.
#
# D3: `Increase` used to require `cpu_load < 0.5` and `io_wait_pct < 15%`,
# both measured while the operation being governed was saturating the member
# disks -- which is exactly when io wait is high. The recovery condition
# could not be satisfied by the system it was measuring, so once any decrease
# landed the speed never came back for the rest of the operation. Combined
# with a load-average comparison that was true on every tick, that is the
# mechanism behind a reshape that ran for days.
#
# Increase is now the decrease thresholds scaled by 0.8, leaving a hysteresis
# band between them, and a decrease is floored at the profile's own minimum
# rather than an absolute 10 MB/s. This case forces a real decrease by
# pushing the machine past the cpu_load threshold, then stops the load and
# keeps ticking -- judged by `cat` of the band's own limits, never by
# shr-rs's report.
#
# What is asserted here is the floor: after every tick the ceiling is still
# inside the profile's own band. The climb-back itself is recorded as
# SKIPPED on a loopback fixture, for the reason SM-THROTTLE-2 already
# documents for the same signal -- see the skip() text at the bottom.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_POINT=/mnt/shr-smoke
# SafetyThresholds::default().max_cpu_load, a fraction of the whole machine.
CPU_LOAD_FRACTION=0.85
RECOVERY_TICKS=4
WORKER_PIDS=()
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

stop_cpu_load_workers() {
    if [[ "${#WORKER_PIDS[@]}" -gt 0 ]]; then
        kill "${WORKER_PIDS[@]}" 2>/dev/null || true
        wait "${WORKER_PIDS[@]}" 2>/dev/null || true
        WORKER_PIDS=()
    fi
}

cleanup() {
    stop_cpu_load_workers
    sudo umount "$MOUNT_POINT" 2>/dev/null || true
    fixture_down >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== SM-THROTTLE-5: arrange (a real reshape to govern) =="
if ! fixture_up 16G 16G 16G; then
    echo "SM-THROTTLE-5: BLOCKED (fixture_up failed)"
    exit 2
fi

CREATE_OUTPUT="$(sudo "$SHR_RS" --json create --mode shr \
    --disks ata-LOOP_DISK_10,ata-LOOP_DISK_11 --mount "$MOUNT_POINT" 2>&1)"
CREATE_EXIT=$?
echo "$CREATE_OUTPUT"
if [[ "$CREATE_EXIT" -ne 0 ]]; then
    echo "SM-THROTTLE-5: BLOCKED (create failed)"
    exit 2
fi

MD_NAME="$(smoke_find_md_for_members loop10p1 loop11p1)"
if [[ -z "$MD_NAME" ]]; then
    echo "SM-THROTTLE-5: BLOCKED (could not find the real md device)"
    exit 2
fi
if ! smoke_wait_sync_idle "$MD_NAME"; then
    echo "SM-THROTTLE-5: BLOCKED (initial resync never finished within the wait window)"
    exit 2
fi

EXPAND_OUTPUT="$(sudo "$SHR_RS" --json expand --add ata-LOOP_DISK_12 --skip-scrub-check 2>&1)"
EXPAND_EXIT=$?
echo "$EXPAND_OUTPUT"
if [[ "$EXPAND_EXIT" -ne 0 ]]; then
    echo "SM-THROTTLE-5: BLOCKED (expand did not succeed)"
    exit 2
fi
if ! smoke_wait_sync_action "$MD_NAME" reshape 12 5; then
    echo "SM-THROTTLE-5: BLOCKED (sync_action never reached 'reshape')"
    exit 2
fi

BASELINE="$(smoke_sync_limit "$MD_NAME" max)"
FLOOR="$(smoke_sync_limit "$MD_NAME" min)"
echo "baseline: max=$BASELINE floor=$FLOOR"

echo "== SM-THROTTLE-5: act 1 (force a real decrease with genuine CPU load) =="
NPROC="$(nproc 2>/dev/null || echo 2)"
CPU_LOAD_THRESHOLD="$(awk -v f="$CPU_LOAD_FRACTION" -v n="$NPROC" 'BEGIN { printf "%.2f", f * n }')"
for _ in $(seq 1 $(( NPROC * 4 ))); do
    ( while :; do :; done ) &
    WORKER_PIDS+=("$!")
done

LOAD_EXCEEDED=0
for attempt in $(seq 1 60); do
    LOAD_NOW="$(awk '{print $1}' /proc/loadavg)"
    if awk -v l="$LOAD_NOW" -v t="$CPU_LOAD_THRESHOLD" 'BEGIN{exit !(l > t)}'; then
        LOAD_EXCEEDED=1
        break
    fi
    (( attempt % 6 == 0 )) && echo "  waiting for load to exceed $CPU_LOAD_THRESHOLD (currently $LOAD_NOW)..."
    sleep 3
done
if [[ "$LOAD_EXCEEDED" -ne 1 ]]; then
    echo "SM-THROTTLE-5: BLOCKED (could not honestly push the load average past $CPU_LOAD_THRESHOLD -- an environment limit, not evidence either way)"
    exit 2
fi

sudo "$SHR_RS" --json internal reshape-throttle-tick >/dev/null 2>&1 || \
    fail "the decreasing tick failed"
DECREASED="$(smoke_sync_limit "$MD_NAME" max)"
echo "after the loaded tick: max=$DECREASED (was $BASELINE, floor $FLOOR)"

if [[ "$BASELINE" -le "$FLOOR" ]]; then
    echo "SM-THROTTLE-5: BLOCKED (the baseline was already at the profile floor, so there is no decrease to recover from)"
    exit 2
fi
[[ "$DECREASED" -lt "$BASELINE" ]] || \
    fail "the tick did not decrease under a genuine cpu_load breach: baseline=$BASELINE after=$DECREASED"
[[ "$DECREASED" -ge "$FLOOR" ]] || \
    fail "the decrease went below the profile's own floor ($DECREASED < $FLOOR) -- at that rate md stutters instead of streaming"

echo "== SM-THROTTLE-5: act 2 (drop the load, keep ticking) =="
stop_cpu_load_workers
# The load average is a 1-minute decay, so the signal the next tick reads
# does not clear the instant the workers die.
echo "waiting for the load average to fall back under $CPU_LOAD_THRESHOLD..."
for attempt in $(seq 1 60); do
    LOAD_NOW="$(awk '{print $1}' /proc/loadavg)"
    awk -v l="$LOAD_NOW" -v t="$CPU_LOAD_THRESHOLD" 'BEGIN{exit !(l < t)}' && break
    (( attempt % 6 == 0 )) && echo "  still $LOAD_NOW..."
    sleep 5
done

CLIMBED=0
CURRENT="$DECREASED"
for tick in $(seq 1 "$RECOVERY_TICKS"); do
    sudo "$SHR_RS" --json internal reshape-throttle-tick >/dev/null 2>&1 || \
        fail "recovery tick $tick failed"
    CURRENT="$(smoke_sync_limit "$MD_NAME" max)"
    echo "recovery tick $tick: max=$CURRENT floor=$(smoke_sync_limit "$MD_NAME" min)"
    if [[ "$CURRENT" -gt "$DECREASED" ]]; then
        CLIMBED=1
        break
    fi
    sleep 5
done

echo "== SM-THROTTLE-5: assert =="
# The floor is re-read here, not reused from the baseline: it moves with the
# capability estimate, which the same ticks above are learning from live
# `sync_speed`. Comparing against the bootstrap floor would fail the moment
# the estimate landed, for a reason that has nothing to do with braking.
FLOOR_NOW="$(smoke_sync_limit "$MD_NAME" min)"
CEILING_NOW="$(awk -v f="$FLOOR_NOW" 'BEGIN { printf "%d", f / 0.35 * 0.75 + 0.5 }')"
echo "limits at assert time: max=$CURRENT min=$FLOOR_NOW"

# This is what the reported multi-day reshape actually needed: the decay has
# a floor at the rate the sync still streams at, instead of an absolute
# 10 MB/s it could never climb back from.
[[ "$CURRENT" -ge "$FLOOR_NOW" ]] || \
    fail "sync_speed_max is $CURRENT, below this band's own floor $FLOOR_NOW -- the decrease is not floored at the profile minimum"
[[ "$CURRENT" -le "$CEILING_NOW" ]] || \
    fail "sync_speed_max is $CURRENT, above the ceiling this profile derives from its own floor ($CEILING_NOW) -- the adaptive loop may not leave the profile's band"

if [[ "$CLIMBED" == 1 ]]; then
    echo "the speed climbed back to $CURRENT once the load cleared"
else
    # Not a failure here, and the reason is the same one SM-THROTTLE-2
    # records for the temperature/SMART brake: on /dev/loop* members
    # `smartctl` cannot read a disk temperature at all, and an unreadable
    # SAFETY-critical signal decreases under every profile by design. So a
    # loopback fixture can never satisfy the increase condition no matter
    # what this script does to the CPU, and asserting a climb-back here
    # would be asserting something this environment cannot produce.
    skip "the climb-back itself cannot be observed on a loopback fixture: /dev/loop* members have no SMART identity, so disk_temp_max is permanently unreadable and every tick decreases regardless of CPU load (the deliberate 'unknown never means safe' rule). What IS asserted above is the half that matters for the reported multi-day reshape -- the decay stops at the profile's own floor instead of an absolute 10 MB/s. The recovery condition itself is covered at the unit layer, where the signal can be injected: throttle.rs's increase_and_decrease_thresholds_leave_a_hysteresis_band and max_climbs_back_after_a_transient_brake."
fi

echo "== SM-THROTTLE-5: cleanup + verify teardown (R4) =="
stop_cpu_load_workers
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-THROTTLE-5: verdict =="
if [[ "${#SKIPPED[@]}" -gt 0 ]]; then
    printf '  skipped: %s\n' "${SKIPPED[@]}"
fi
if [[ "$RESULT" == PASS ]]; then
    echo "SM-THROTTLE-5: PASS"
else
    printf 'SM-THROTTLE-5: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
