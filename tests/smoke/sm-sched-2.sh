#!/usr/bin/env bash
# SM-SCHED-2: `schedule install` with 4 real groups must produce 4 distinct,
# correct per-group scrub timers that coexist without overwriting each
# other. This exists specifically to catch the regression class Phase 4 had
# for real: `write_managed_configs`/mdadm.conf/fstab regenerated from only
# the group just touched, silently deleting every other group's entry (see
# conf.rs's module doc comment and sm-groups-4.sh's own header). The unit
# writer's fix was structural -- one file PER GROUP, named after that group,
# so writing group B's units never even reads group A's -- but "structurally
# should be impossible" is exactly the kind of claim this suite exists to
# verify against real behavior instead of trusting the design.
#
# Each group here is a small, single-band 3-disk RAID5 -- deliberately
# uniform and minimal (no heterogeneous bands, no resync-completion wait):
# SM-GROUPS-4 already proves multi-group array correctness in depth; this
# script's only job is the scheduler's per-group file independence, so it
# creates the minimum real state that gives `schedule install` something
# genuine to iterate over.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
UNIT_DIR=/etc/systemd/system
RESULT=PASS
FAILURES=()

fail() {
    RESULT=FAIL
    FAILURES+=("$1")
    echo "FAIL: $1" >&2
}

SCHED_GROUPS=(sched-g1 sched-g2 sched-g3 sched-g4)
declare -A DISKS=(
    [sched-g1]="ata-LOOP_DISK_10,ata-LOOP_DISK_11,ata-LOOP_DISK_12"
    [sched-g2]="ata-LOOP_DISK_13,ata-LOOP_DISK_14,ata-LOOP_DISK_15"
    [sched-g3]="ata-LOOP_DISK_16,ata-LOOP_DISK_17,ata-LOOP_DISK_18"
    [sched-g4]="ata-LOOP_DISK_19,ata-LOOP_DISK_20,ata-LOOP_DISK_21"
)
declare -A VG=(
    [sched-g1]=shr_vg_sched1 [sched-g2]=shr_vg_sched2
    [sched-g3]=shr_vg_sched3 [sched-g4]=shr_vg_sched4
)
declare -A MOUNT=(
    [sched-g1]=/mnt/sched-g1 [sched-g2]=/mnt/sched-g2
    [sched-g3]=/mnt/sched-g3 [sched-g4]=/mnt/sched-g4
)

ALL_TIMER_NAMES=(shr-rs-throttle-tick shr-rs-health-check)
for g in "${SCHED_GROUPS[@]}"; do
    ALL_TIMER_NAMES+=("shr-rs-scrub-$g")
done

cleanup() {
    for t in "${ALL_TIMER_NAMES[@]}"; do
        sudo systemctl disable --now "$t.timer" 2>/dev/null || true
        sudo rm -f "$UNIT_DIR/$t.timer" "$UNIT_DIR/$t.service"
    done
    sudo systemctl daemon-reload 2>/dev/null || true
    for g in "${SCHED_GROUPS[@]}"; do
        sudo umount "${MOUNT[$g]}" 2>/dev/null || true
    done
    fixture_down >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== SM-SCHED-2: arrange (4 real, independent SHR groups) =="
if ! fixture_up 8G 8G 8G 8G 8G 8G 8G 8G 8G 8G 8G 8G; then
    echo "SM-SCHED-2: BLOCKED (fixture_up failed)"
    exit 2
fi

for g in "${SCHED_GROUPS[@]}"; do
    echo "-- creating group $g (disks=${DISKS[$g]}) --"
    OUTPUT="$(sudo "$SHR_RS" --json create --mode shr --disks "${DISKS[$g]}" \
        --name "$g" --vg-name "${VG[$g]}" --mount "${MOUNT[$g]}" 2>&1)"
    CREATE_EXIT=$?
    echo "$OUTPUT"
    if [[ "$CREATE_EXIT" -ne 0 ]]; then
        echo "SM-SCHED-2: BLOCKED (create failed for $g)"
        exit 2
    fi
done

echo "== SM-SCHED-2: act (schedule install, once, across all 4 groups) =="
INSTALL_OUTPUT="$(sudo "$SHR_RS" --json schedule install 2>&1)"
INSTALL_EXIT=$?
echo "$INSTALL_OUTPUT"
[[ "$INSTALL_EXIT" -eq 0 ]] || fail "schedule install should have succeeded across 4 groups"

echo "== SM-SCHED-2: assert (independent systemd/filesystem state observation) =="

LIST_TIMERS="$(systemctl list-timers --all --no-legend 2>/dev/null || true)"
echo "$LIST_TIMERS"

for t in "${ALL_TIMER_NAMES[@]}"; do
    [[ -f "$UNIT_DIR/$t.service" ]] || fail "$t.service missing from $UNIT_DIR"
    [[ -f "$UNIT_DIR/$t.timer" ]] || fail "$t.timer missing from $UNIT_DIR"
    echo "$LIST_TIMERS" | grep -qF "$t.timer" || fail "$t.timer does not appear in 'systemctl list-timers --all'"
done

echo "-- each group's own scrub service names ONLY that group, and none of the"
echo "   other 3 group names leaked into it (the multi-group overwrite trap) --"
for g in "${SCHED_GROUPS[@]}"; do
    service_file="$UNIT_DIR/shr-rs-scrub-$g.service"
    [[ -f "$service_file" ]] || continue
    CONTENT="$(sudo cat "$service_file")"
    echo "$CONTENT" | grep -qF -- "--name $g" || fail "shr-rs-scrub-$g.service's ExecStart= does not target group '$g': $(echo "$CONTENT" | grep ExecStart)"
    for other in "${SCHED_GROUPS[@]}"; do
        [[ "$other" == "$g" ]] && continue
        echo "$CONTENT" | grep -qF -- "--name $other" && \
            fail "shr-rs-scrub-$g.service ALSO mentions group '$other' -- cross-group overwrite/collision"
    done
done

echo "-- exactly 4 distinct shr-rs-scrub-*.service files, one per group --"
SCRUB_SERVICE_COUNT="$(find "$UNIT_DIR" -maxdepth 1 -name 'shr-rs-scrub-sched-g*.service' 2>/dev/null | wc -l)"
[[ "$SCRUB_SERVICE_COUNT" == 4 ]] || fail "expected exactly 4 shr-rs-scrub-sched-g*.service files, found $SCRUB_SERVICE_COUNT"

echo "== SM-SCHED-2: cleanup + verify teardown (R4) =="
for t in "${ALL_TIMER_NAMES[@]}"; do
    sudo systemctl disable --now "$t.timer" 2>/dev/null || true
    sudo rm -f "$UNIT_DIR/$t.timer" "$UNIT_DIR/$t.service"
done
sudo systemctl daemon-reload 2>/dev/null || true
for g in "${SCHED_GROUPS[@]}"; do
    sudo umount "${MOUNT[$g]}" 2>/dev/null || true
done
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-SCHED-2: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-SCHED-2: PASS"
else
    printf 'SM-SCHED-2: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
