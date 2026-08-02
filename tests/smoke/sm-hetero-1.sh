#!/usr/bin/env bash
# SM-HETERO-1: heterogeneous SHR create on real loopback devices, scaled
# down from the the design's canonical [3,3,4,6] TB example.
#
# Sizes are 16G/16G/24G/40G, NOT a literal /1000 scale of 3/3/4/6 TB: the
# planner's `band_alignment` (4 GiB) is an absolute constant that does not
# shrink with the test. At TB scale it's a negligible fraction of any disk;
# naively used at raw GB scale (3G/3G/4G/6G) it consumes the ENTIRE usable
# capacity of the two smaller disks (align_down(~2.9G, 4G) == 0), so no band
# can form at all -- discovered by actually running this against real
# fixture disks, not by inspection. 16G/16G/24G/40G keeps the same band
# shape (all 4 disks in band0, only the two largest in band1, an unusable
# single-disk remainder) while keeping alignment loss a small fraction:
#   usable(16G) = align_down(16G - 136M, 4G)  = 12G  (x2, tied smallest)
#   usable(24G) = align_down(24G - 136M, 4G)  = 20G
#   usable(40G) = align_down(40G - 136M, 4G)  = 36G
#   band0 [0,12G)  members=all 4           -> RAID5, 4 members
#   band1 [12G,20G) members={24G,40G disks} -> RAID1, 2 members
#   [20G,36G) members={40G disk only}       -> unusable (SHR needs 2+)
#
# Scope: this case verifies partition/mdadm band-membership correctness
# only (D2, D9) -- it does not require the LVM/Btrfs stages to succeed, and
# deliberately does not assert on `shr-rs create`'s exit code either way.
# Before Phase 4 Step 8 this guest had no Btrfs at all, so the LVM/Btrfs
# stages were expected to fail here every time; since Step 8 (ELRepo
# kernel-lt, see the design) they're expected to succeed.
# Either way this case's own verdict is unaffected -- judgment is by kernel
# state (/proc/mdstat, lsblk), never by shr-rs's own exit code or printed
# message. The LVM/Btrfs/mount stages
# themselves are covered by SM-FULLSTACK-1.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
RESULT=PASS
FAILURES=()

fail() {
    RESULT=FAIL
    FAILURES+=("$1")
    echo "FAIL: $1" >&2
}

cleanup() { fixture_down >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo "== SM-HETERO-1: arrange =="
if ! fixture_up 16G 16G 24G 40G; then
    echo "SM-HETERO-1: BLOCKED (fixture_up failed)"
    exit 2
fi
DISKS="ata-LOOP_DISK_10,ata-LOOP_DISK_11,ata-LOOP_DISK_12,ata-LOOP_DISK_13"

echo "-- sanity: before state --"
cat /proc/mdstat
lsblk /dev/loop10 /dev/loop11 /dev/loop12 /dev/loop13

echo "== SM-HETERO-1: act =="
sudo "$SHR_RS" --json create --mode shr --disks "$DISKS" --mount /mnt/shr-smoke \
    | tee /tmp/shr-hetero-create.json
CREATE_EXIT=${PIPESTATUS[0]}
echo "shr-rs create exit code: $CREATE_EXIT (nonzero from the expected mkfs.btrfs failure is fine here -- see header)"

echo "== SM-HETERO-1: assert (independent kernel state observation) =="

MDSTAT="$(cat /proc/mdstat)"
echo "$MDSTAT"

echo "$MDSTAT" | grep -q "raid5" || fail "no raid5 array (band0) in /proc/mdstat"
MD0_LINE="$(echo "$MDSTAT" | grep '^md0' || true)"
for d in loop10p1 loop11p1 loop12p1 loop13p1; do
    echo "$MD0_LINE" | grep -q "$d" || fail "md0 (band0) missing member $d -- band0 must span all 4 disks"
done
# Exact count, not just presence: a member grep alone would not notice an
# extra, unexpected 5th member joining the array.
MD0_MEMBER_COUNT="$(echo "$MD0_LINE" | grep -oE 'loop1[0-9]p[0-9]+\[[0-9]+\]' | wc -l)"
[[ "$MD0_MEMBER_COUNT" == 4 ]] || fail "md0 has $MD0_MEMBER_COUNT members, expected exactly 4"

echo "$MDSTAT" | grep -q "raid1" || fail "no raid1 array (band1) in /proc/mdstat -- did band1 fail to form?"
MD1_LINE="$(echo "$MDSTAT" | grep '^md1' || true)"
echo "$MD1_LINE" | grep -q "loop12p2" || fail "band1 (md1) missing loop12p2 (24G disk)"
echo "$MD1_LINE" | grep -q "loop13p2" || fail "band1 (md1) missing loop13p2 (40G disk)"
MD1_MEMBER_COUNT="$(echo "$MD1_LINE" | grep -oE 'loop1[0-9]p[0-9]+\[[0-9]+\]' | wc -l)"
[[ "$MD1_MEMBER_COUNT" == 2 ]] || fail "md1 has $MD1_MEMBER_COUNT members, expected exactly 2"

# This is the exact D2 regression shape (every disk partitioned for every
# band) -- note that with D2 actually reintroduced, loop10/loop11 would get
# a p2 partition past the end of their device, so real parted would abort
# create() before md1 ever formed at all; these greps are a second net in
# case some other regression put the wrong disk in band1 while band0/md1's
# existence checks above still pass.
echo "$MD1_LINE" | grep -q "loop10" && fail "band1 (md1) includes loop10 (16G disk) -- D2 regression"
echo "$MD1_LINE" | grep -q "loop11" && fail "band1 (md1) includes loop11 (16G disk) -- D2 regression"

for d in loop10 loop11; do
    if lsblk -no NAME "/dev/${d}" | grep -q "${d}p2"; then
        fail "$d has an unexpected p2 partition (should be band0-only: p1)"
    fi
done

if lsblk -no NAME "/dev/loop13" | grep -q "loop13p3"; then
    fail "loop13 (40G) has an unexpected p3 -- the ~16G unusable tail must stay unpartitioned"
fi

# D9: partition start offsets must match reserved_head (128 MiB = sector
# 262144) and band1's offset (128 MiB + 12 GiB = sector 25427968) exactly,
# read straight from the kernel (sysfs), not from shr-rs's own state.toml
# or JSON output.
EXPECTED_P1_START=262144
EXPECTED_P2_START=25427968
for d in loop12 loop13; do
    p1_start="$(cat "/sys/class/block/${d}p1/start" 2>/dev/null || echo MISSING)"
    [[ "$p1_start" == "$EXPECTED_P1_START" ]] || \
        fail "${d}p1 starts at sector $p1_start, expected $EXPECTED_P1_START (reserved_head)"
    p2_start="$(cat "/sys/class/block/${d}p2/start" 2>/dev/null || echo MISSING)"
    [[ "$p2_start" == "$EXPECTED_P2_START" ]] || \
        fail "${d}p2 starts at sector $p2_start, expected $EXPECTED_P2_START (band1 offset)"
done

echo "== SM-HETERO-1: cleanup + verify teardown (R4) =="
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-HETERO-1: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-HETERO-1: PASS"
else
    printf 'SM-HETERO-1: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
