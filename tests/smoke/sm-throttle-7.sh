#!/usr/bin/env bash
# SM-THROTTLE-7: two groups scrubbing at different profiles at the same time
# each keep their own limits.
#
# This is the property the host-wide parameter could not express at all.
# `/proc/sys/dev/raid/speed_limit_max` is one number for the whole machine,
# so a second group's scrub started at a different profile simply overwrote
# the first group's setting, silently, with nothing in shr-rs reporting it.
# The per-array `sync_speed_{min,max}` attributes shadow the host-wide pair
# for one array alone, which is what makes a per-band profile mean anything.
#
# Judged by reading each band's own attributes while BOTH checks are running.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_A=/mnt/shr-smoke-a
MOUNT_B=/mnt/shr-smoke-b
# throttle.rs's UNBOUNDED_SPEED_KB, what `max` writes to both limits.
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
    sudo umount "$MOUNT_A" 2>/dev/null || true
    sudo umount "$MOUNT_B" 2>/dev/null || true
    fixture_down >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== SM-THROTTLE-7: arrange (two independent groups, two disks each) =="
if ! fixture_up 8G 8G 8G 8G; then
    echo "SM-THROTTLE-7: BLOCKED (fixture_up failed)"
    exit 2
fi

create_group() {
    local name="$1" disks="$2" mount="$3" vg="$4"
    local out exit_code
    out="$(sudo "$SHR_RS" --json create --mode shr --disks "$disks" --name "$name" \
        --vg-name "$vg" --mount "$mount" 2>&1)"
    exit_code=$?
    echo "$out"
    return $exit_code
}

if ! create_group alpha ata-LOOP_DISK_10,ata-LOOP_DISK_11 "$MOUNT_A" shr_vg_alpha; then
    echo "SM-THROTTLE-7: BLOCKED (creating group alpha failed)"
    exit 2
fi
if ! create_group beta ata-LOOP_DISK_12,ata-LOOP_DISK_13 "$MOUNT_B" shr_vg_beta; then
    echo "SM-THROTTLE-7: BLOCKED (creating group beta failed)"
    exit 2
fi

MD_A="$(smoke_find_md_for_members loop10p1 loop11p1)"
MD_B="$(smoke_find_md_for_members loop12p1 loop13p1)"
if [[ -z "$MD_A" || -z "$MD_B" ]]; then
    echo "SM-THROTTLE-7: BLOCKED (could not find both md devices: alpha='$MD_A' beta='$MD_B')"
    exit 2
fi
echo "alpha is /dev/$MD_A, beta is /dev/$MD_B"

if [[ "$(smoke_sync_limit_origin "$MD_A" max)" == absent ]]; then
    skip "this kernel has no per-array sync_speed_max, so two groups genuinely cannot hold different limits at once and there is nothing to assert"
    echo "SM-THROTTLE-7: SKIPPED (no per-array limit attributes on this kernel)"
    exit 0
fi

for md in "$MD_A" "$MD_B"; do
    if ! smoke_wait_sync_idle "$md"; then
        echo "SM-THROTTLE-7: BLOCKED ($md's initial resync never finished)"
        exit 2
    fi
done

echo "== SM-THROTTLE-7: act (scrub alpha at max, beta at background, both live) =="
sudo "$SHR_RS" --json fs scrub start --name alpha --priority max 2>&1 || \
    fail "starting alpha's scrub failed"
sudo "$SHR_RS" --json fs scrub start --name beta --priority background 2>&1 || \
    fail "starting beta's scrub failed"

for md in "$MD_A" "$MD_B"; do
    ACTION="$(cat "/sys/block/$md/md/sync_action" 2>/dev/null || echo unknown)"
    [[ "$ACTION" == "check" ]] || \
        fail "$md's sync_action is '$ACTION', expected 'check' -- both scrubs must really be running at once for this comparison to mean anything"
done

echo "== SM-THROTTLE-7: assert (each band kept its own limits) =="
A_MAX="$(smoke_sync_limit "$MD_A" max)"
A_MIN="$(smoke_sync_limit "$MD_A" min)"
B_MAX="$(smoke_sync_limit "$MD_B" max)"
B_MIN="$(smoke_sync_limit "$MD_B" min)"
echo "alpha (max profile):        max=$A_MAX min=$A_MIN"
echo "beta  (background profile): max=$B_MAX min=$B_MIN"

[[ "$A_MAX" == "$UNBOUNDED_KB" && "$A_MIN" == "$UNBOUNDED_KB" ]] || \
    fail "alpha's limits are max=$A_MAX min=$A_MIN, expected the unbounded sentinel $UNBOUNDED_KB on both -- beta's later scrub start appears to have overwritten them"
[[ "$B_MAX" != "$UNBOUNDED_KB" ]] || \
    fail "beta's ceiling is the unbounded sentinel, so it took alpha's max profile instead of its own background one"
[[ "$B_MAX" -lt "$A_MAX" ]] || \
    fail "beta's ceiling ($B_MAX) is not below alpha's ($A_MAX), so the two profiles are indistinguishable in the kernel"

echo "== SM-THROTTLE-7: cleanup + verify teardown (R4) =="
sudo "$SHR_RS" --json fs scrub cancel --name alpha >/dev/null 2>&1 || true
sudo "$SHR_RS" --json fs scrub cancel --name beta >/dev/null 2>&1 || true
sudo umount "$MOUNT_A" 2>/dev/null || true
sudo umount "$MOUNT_B" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-THROTTLE-7: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-THROTTLE-7: PASS"
else
    printf 'SM-THROTTLE-7: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
