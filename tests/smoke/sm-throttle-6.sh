#!/usr/bin/env bash
# SM-THROTTLE-6: `fs scrub start --priority max` writes the unbounded
# sentinel to BOTH of the band's own limits, and clears them to `system` when
# the check finishes.
#
# D1, the direct cause of the reported "max scrub was still throttled": the
# scrub path wrote a ceiling and no floor, so it inherited whatever floor was
# in place -- after any prior `expand`, a fixed 1 MB/s. The kernel reduces
# the sync rate toward `sync_speed_min` whenever non-sync IO touches the
# member devices, and on a live NAS there always is some, so `--priority max`
# raised a limit the operation never reached while the floor pinned it.
#
# `max` means the absence of any artificial limit, and at this interface that
# has to be written as a number above anything the hardware can reach --
# `UNBOUNDED_SPEED_KB`. A floor md can never fall back to is exactly what
# "the operator accepted that everyday IO will be slower" means.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_POINT=/mnt/shr-smoke
# throttle.rs's UNBOUNDED_SPEED_KB.
UNBOUNDED_KB=10000000
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

cleanup() {
    sudo umount "$MOUNT_POINT" 2>/dev/null || true
    fixture_down >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== SM-THROTTLE-6: arrange (a real 3-disk RAID5 to scrub) =="
if ! fixture_up 8G 8G 8G; then
    echo "SM-THROTTLE-6: BLOCKED (fixture_up failed)"
    exit 2
fi

CREATE_OUTPUT="$(sudo "$SHR_RS" --json create --mode shr \
    --disks ata-LOOP_DISK_10,ata-LOOP_DISK_11,ata-LOOP_DISK_12 --mount "$MOUNT_POINT" 2>&1)"
CREATE_EXIT=$?
echo "$CREATE_OUTPUT"
if [[ "$CREATE_EXIT" -ne 0 ]]; then
    echo "SM-THROTTLE-6: BLOCKED (create failed)"
    exit 2
fi

MD_NAME="$(smoke_find_md_for_members loop10p1 loop11p1 loop12p1)"
if [[ -z "$MD_NAME" ]]; then
    echo "SM-THROTTLE-6: BLOCKED (could not find the real md device for band0)"
    exit 2
fi
if [[ "$(smoke_sync_limit_origin "$MD_NAME" max)" == absent ]]; then
    skip "this kernel has no per-array sync_speed_max, so there is nothing per-band to assert against"
    echo "SM-THROTTLE-6: SKIPPED (no per-array limit attributes on this kernel)"
    exit 0
fi
if ! smoke_wait_sync_idle "$MD_NAME"; then
    echo "SM-THROTTLE-6: BLOCKED (initial resync never finished within the wait window)"
    exit 2
fi

echo "== SM-THROTTLE-6: act (fs scrub start --priority max) =="
SCRUB_OUTPUT="$(sudo "$SHR_RS" --json fs scrub start --priority max 2>&1)"
SCRUB_EXIT=$?
echo "$SCRUB_OUTPUT"
[[ "$SCRUB_EXIT" -eq 0 ]] || fail "fs scrub start --priority max should have succeeded"

echo "== SM-THROTTLE-6: assert 1 (both limits carry the sentinel) =="
for which in min max; do
    VALUE="$(smoke_sync_limit "$MD_NAME" "$which")"
    ORIGIN="$(smoke_sync_limit_origin "$MD_NAME" "$which")"
    echo "sync_speed_$which during a --priority max scrub: $VALUE ($ORIGIN)"
    [[ "$VALUE" == "$UNBOUNDED_KB" ]] || \
        fail "sync_speed_$which is $VALUE during a --priority max scrub, expected the unbounded sentinel $UNBOUNDED_KB -- a floor below what the array can do is the kernel's licence to slow this scrub down under any everyday IO, which is the whole defect"
    [[ "$ORIGIN" == local ]] || \
        fail "sync_speed_$which reads '$ORIGIN', so this scrub's profile did not reach the band's own attributes"
done

SYNC_ACTION="$(cat "/sys/block/$MD_NAME/md/sync_action" 2>/dev/null || echo unknown)"
echo "sync_action after scrub start: $SYNC_ACTION"
[[ "$SYNC_ACTION" == "check" ]] || \
    fail "sync_action is '$SYNC_ACTION' right after 'fs scrub start', expected 'check' -- the limits above would prove nothing if no scrub actually started"

echo "== SM-THROTTLE-6: act 2 (end the check, then one throttle tick) =="
sudo "$SHR_RS" --json fs scrub cancel >/dev/null 2>&1 || true
if ! smoke_wait_sync_idle "$MD_NAME" 30 2; then
    echo "SM-THROTTLE-6: BLOCKED (the check never went back to idle)"
    exit 2
fi
sudo "$SHR_RS" --json internal reshape-throttle-tick >/dev/null 2>&1 || true

echo "== SM-THROTTLE-6: assert 2 (the limits are cleared, not left behind) =="
for which in min max; do
    ORIGIN="$(smoke_sync_limit_origin "$MD_NAME" "$which")"
    echo "sync_speed_$which after the check finished: $(smoke_sync_limit "$MD_NAME" "$which") ($ORIGIN)"
    [[ "$ORIGIN" == system ]] || \
        fail "sync_speed_$which still reads '$ORIGIN' -- an unbounded floor left behind on this array governs every later operation on it"
done

echo "== SM-THROTTLE-6: cleanup + verify teardown (R4) =="
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-THROTTLE-6: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-THROTTLE-6: PASS"
else
    printf 'SM-THROTTLE-6: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
