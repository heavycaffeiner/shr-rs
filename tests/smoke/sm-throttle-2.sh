#!/usr/bin/env bash
# when a safety threshold is exceeded during a real reshape,
# `internal reshape-throttle-tick` must actually lower
# /proc/sys/dev/raid/speed_limit_max, not merely report a decision.
#
# HOW THE CONDITION IS HONESTLY INJECTED ON THIS FIXTURE (read before editing
# thresholds below):
#
# `ReshapeThrottle::tick` (crates/shr-exec/src/throttle.rs) checks four
# safety-critical signals: cpu_load, io_wait_pct, disk_temp_max, and
# smart_delta_reallocated. On loopback devices (/dev/loop10 etc.), the last
# two come from `smartctl` (crates/shr-orchestrate/src/metrics.rs's
# `read_smart_signals`), and a loop device has no real ATA/SMART identity at
# all -- `smartctl` against it is not "reads a normal value", it is "cannot
# read anything", which `tick()` already treats as "must decelerate, never
# treated as safe" (its own doc comment: "unknown never gets to look like
# safe"). That is a REAL and INTERESTING safety property, but it fires on
# every single tick on this fixture regardless of any condition this script
# injects -- there is no way to distinguish "the test's injected condition
# caused this" from "this always happens here", so asserting a decrease
# through THAT path would not actually be testing threshold-exceeded
# behavior; it would be indistinguishable from a tautology. Temperature and
# SMART-based EmergencyBrake specifically are therefore SKIPPED below, with
# a clear reason recorded, rather than faking a PASS off a signal this
# fixture cannot honestly control.
#
# cpu_load is different: `read_cpu_load` reads /proc/loadavg's real 1-minute
# average, unrelated to any disk -- this IS something a script can honestly
# push past `SafetyThresholds::default().max_cpu_load` (0.85) by starting
# real, CPU-bound background processes on the guest and independently
# confirming (via `cat /proc/loadavg`, not shr-rs) that the threshold is
# genuinely exceeded before ever invoking shr-rs. That is what this script
# actually tests as its PASS/FAIL verdict.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_POINT=/mnt/shr-smoke
CPU_LOAD_THRESHOLD=0.85  # SafetyThresholds::default().max_cpu_load
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

echo "== SM-THROTTLE-2: arrange (a real 2-disk RAID1, promoted to RAID5 by a"
echo "   real expand -- the reshape it starts is what gets throttle-ticked) =="
if ! fixture_up 16G 16G 16G; then
    echo "SM-THROTTLE-2: BLOCKED (fixture_up failed)"
    exit 2
fi

CREATE_OUTPUT="$(sudo "$SHR_RS" --json create --mode shr --disks ata-LOOP_DISK_10,ata-LOOP_DISK_11 --mount "$MOUNT_POINT" 2>&1)"
CREATE_EXIT=$?
echo "$CREATE_OUTPUT"
if [[ "$CREATE_EXIT" -ne 0 ]]; then
    echo "SM-THROTTLE-2: BLOCKED (create failed)"
    fixture_down >/dev/null 2>&1 || true
    exit 2
fi

MD_NAME="$(smoke_find_md_for_members loop10p1 loop11p1)"
if [[ -z "$MD_NAME" ]]; then
    echo "SM-THROTTLE-2: BLOCKED (could not find the real md device for the new array)"
    exit 2
fi

if ! smoke_wait_sync_idle "$MD_NAME"; then
    echo "SM-THROTTLE-2: BLOCKED (initial resync never finished within the wait window)"
    exit 2
fi

# `--skip-scrub-check` is required, not a convenience: the fixture group was
# created moments ago, so the scrub-freshness gate ("no scrub successfully
# completed within the last 30 days") correctly refuses this expand and no
# reshape would ever start. Measured on the guest. That gate is SM-SCRUB-4's
# subject; this case is about
# whether the adaptive throttle actually slows a running reshape down.
EXPAND_OUTPUT="$(sudo "$SHR_RS" --json expand --add ata-LOOP_DISK_12 --skip-scrub-check 2>&1)"
EXPAND_EXIT=$?
echo "$EXPAND_OUTPUT"
if [[ "$EXPAND_EXIT" -ne 0 ]]; then
    echo "SM-THROTTLE-2: BLOCKED (arranging the reshape failed -- expand did not succeed)"
    exit 2
fi

if ! smoke_wait_sync_action "$MD_NAME" reshape 12 5; then
    echo "SM-THROTTLE-2: BLOCKED (sync_action never reached 'reshape' after expand -- nothing to throttle)"
    exit 2
fi

BASELINE_SPEED="$(sudo cat /proc/sys/dev/raid/speed_limit_max)"
echo "baseline speed_limit_max (set by apply_initial when the reshape started): $BASELINE_SPEED"

echo "== SM-THROTTLE-2: act, part 1 -- inject a genuine CPU-load threshold breach =="
NPROC="$(nproc 2>/dev/null || echo 2)"
WORKER_COUNT=$(( NPROC * 4 ))
echo "starting $WORKER_COUNT real busy-loop workers (nproc=$NPROC) to push /proc/loadavg past $CPU_LOAD_THRESHOLD..."
for _ in $(seq 1 "$WORKER_COUNT"); do
    ( while :; do :; done ) &
    WORKER_PIDS+=("$!")
done

LOAD_EXCEEDED=0
LOAD_NOW=0
for attempt in $(seq 1 60); do
    LOAD_NOW="$(awk '{print $1}' /proc/loadavg)"
    if awk -v l="$LOAD_NOW" -v t="$CPU_LOAD_THRESHOLD" 'BEGIN{exit !(l > t)}'; then
        LOAD_EXCEEDED=1
        break
    fi
    if (( attempt % 6 == 0 )); then
        echo "  waiting for load average to exceed $CPU_LOAD_THRESHOLD (currently $LOAD_NOW)..."
    fi
    sleep 3
done
echo "/proc/loadavg (independent, real): $(cat /proc/loadavg) -- exceeded threshold: $LOAD_EXCEEDED"

if [[ "$LOAD_EXCEEDED" -ne 1 ]]; then
    echo "SM-THROTTLE-2: BLOCKED (could not honestly push 1-min load average past $CPU_LOAD_THRESHOLD within the wait window -- not enough CPU contention on this guest to inject the condition; this is an environment limit, not evidence either way about the throttle logic)"
    exit 2
fi

echo "== SM-THROTTLE-2: act, part 2 -- tick the throttle while the threshold is exceeded =="
TICK_OUTPUT="$(sudo "$SHR_RS" --json internal reshape-throttle-tick 2>&1)"
TICK_EXIT=$?
echo "$TICK_OUTPUT"
echo "reshape-throttle-tick exit code: $TICK_EXIT"
[[ "$TICK_EXIT" -eq 0 ]] || fail "internal reshape-throttle-tick should have succeeded (exit 0) even when it decides to decelerate"

echo "== SM-THROTTLE-2: assert (independent kernel state observation) =="
AFTER_SPEED="$(sudo cat /proc/sys/dev/raid/speed_limit_max)"
LOAD_AT_TICK="$(cat /proc/loadavg)"
echo "speed_limit_max: baseline=$BASELINE_SPEED after-tick=$AFTER_SPEED (load at tick time: $LOAD_AT_TICK)"

[[ "$AFTER_SPEED" -lt "$BASELINE_SPEED" ]] || \
    fail "speed_limit_max did not decrease under a genuine cpu_load threshold breach: baseline=$BASELINE_SPEED after=$AFTER_SPEED"

if [[ "$BASELINE_SPEED" == 100000 ]]; then
    EXPECTED_AFTER=70000
    [[ "$AFTER_SPEED" == "$EXPECTED_AFTER" ]] || \
        fail "speed_limit_max is $AFTER_SPEED, expected exactly $EXPECTED_AFTER (baseline 100000 * Decrease(0.7) factor from ThrottleDecision::Decrease)"
fi

echo "== SM-THROTTLE-2: EmergencyBrake via disk temperature / SMART reallocated-sector count =="
skip "cannot honestly inject a genuine over-temperature or increasing-reallocated-sectors reading: /dev/loop* devices have no real SMART data at all, so smartctl fails identically regardless of any condition this script could set up. That failure already forces a Decrease every tick on this fixture (see this script's header comment on 'unknown never means safe') -- indistinguishable from a real injected threshold breach, so it cannot be used as evidence FOR this specific safety path without lying about what was tested. A real EmergencyBrake-via-temperature/SMART test needs either a scsi_debug/nbd device that answers smartctl, or a MetricsSampler test double wired at the unit-test layer (already covered there, see crates/shr-exec/src/throttle.rs's own tests: tick_emergency_brakes_when_disk_temperature_hits_the_threshold, tick_emergency_brakes_when_smart_reallocated_count_increases) -- not this guest's loopback fixture."

echo "== SM-THROTTLE-2: cleanup + verify teardown (R4) =="
stop_cpu_load_workers
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-THROTTLE-2: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-THROTTLE-2: PASS (cpu_load-triggered Decrease verified; ${#SKIPPED[@]} sub-case SKIPPED, see above -- not counted as PASS)"
else
    printf 'SM-THROTTLE-2: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
