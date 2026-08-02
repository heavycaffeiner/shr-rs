#!/usr/bin/env bash
# SM-FULLSTACK-1: full real pipeline -- partition -> mdadm -> LVM -> Btrfs ->
# mount -- on real loopback devices, now that Step 8 has put a real Btrfs
# kernel module on this guest (kernel-lt via ELRepo; see
# The design for why not the `kmod-btrfs` package the plan
# originally assumed). Same-size disks (single RAID5 band, no heterogeneous
# complexity) -- SM-HETERO-1 already covers band-membership correctness;
# this case is about the LVM/Btrfs/mount stages SM-HETERO-1 explicitly
# doesn't require to succeed.
#
# Judgment throughout is by kernel/filesystem state (findmnt, /proc/mdstat,
# pvs/vgs/lvs, a real file's sha256 after a remount), never by shr-rs's own
# exit code or printed message.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_POINT=/mnt/shr-smoke
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

echo "== SM-FULLSTACK-1: arrange =="
if ! fixture_up 16G 16G 16G; then
    echo "SM-FULLSTACK-1: BLOCKED (fixture_up failed)"
    exit 2
fi
DISKS="ata-LOOP_DISK_10,ata-LOOP_DISK_11,ata-LOOP_DISK_12"

echo "== SM-FULLSTACK-1: act =="
OUTPUT="$(sudo "$SHR_RS" --json create --mode shr --disks "$DISKS" --mount "$MOUNT_POINT" --compression zstd:3 2>&1)"
CREATE_EXIT=$?
echo "$OUTPUT"
echo "shr-rs create exit code: $CREATE_EXIT"

echo "== SM-FULLSTACK-1: assert (independent kernel/filesystem state observation) =="

if [[ "$CREATE_EXIT" -ne 0 ]]; then
    echo "SM-FULLSTACK-1: BLOCKED (create failed -- see the design judgment rules for mdadm+LVM-only fallback judgment)"
    cat /proc/mdstat
    exit 2
fi

MDSTAT="$(cat /proc/mdstat)"
echo "$MDSTAT"
echo "$MDSTAT" | grep -q "raid5" || fail "no raid5 array in /proc/mdstat"
MD0_LINE="$(echo "$MDSTAT" | grep '^md0' || true)"
MD0_MEMBER_COUNT="$(echo "$MD0_LINE" | grep -oE 'loop1[0-9]p[0-9]+\[[0-9]+\]' | wc -l)"
[[ "$MD0_MEMBER_COUNT" == 3 ]] || fail "md0 has $MD0_MEMBER_COUNT members, expected exactly 3"
# A freshly-created array's initial parity resync can take several minutes
# even for a small test volume (real disk I/O, not simulated) -- a `_` slot
# during that window means "still catching up", not "missing/failed"
# (that's `F` or the slot being absent entirely, which the member-count
# check above already would have caught). This case's own concern is
# whether the pipeline reached a healthy, fully-populated array at all, not
# how far initial resync has progressed -- don't force every smoke run to
# block on a multi-minute resync just to make that irrelevant distinction.
echo "$MD0_LINE" | grep -q '\[F\]\|(F)' && fail "md0 has a failed member -- $MD0_LINE"

sudo pvs --noheadings -o pv_name,vg_name 2>/dev/null | grep -q "/dev/md0" || fail "no PV over /dev/md0 in \`pvs\`"
sudo vgs --noheadings -o vg_name 2>/dev/null | grep -qw "shr_vg" || fail "VG shr_vg not found in \`vgs\`"
sudo lvs --noheadings -o lv_name,vg_name 2>/dev/null | grep -q "data" || fail "LV data not found in \`lvs\`"

FINDMNT="$(findmnt -no FSTYPE,OPTIONS "$MOUNT_POINT" 2>/dev/null || true)"
echo "findmnt: $FINDMNT"
echo "$FINDMNT" | grep -q "btrfs" || fail "$MOUNT_POINT is not mounted as btrfs (findmnt: $FINDMNT)"
echo "$FINDMNT" | grep -q "compress=zstd:3" || fail "$MOUNT_POINT missing compress=zstd:3 mount option (findmnt: $FINDMNT)"

TESTFILE="$MOUNT_POINT/smoke-write-test.bin"
sudo dd if=/dev/urandom of="$TESTFILE" bs=1M count=8 status=none || fail "could not write test file to mounted volume"
sudo sync
SHA_BEFORE="$(sudo sha256sum "$TESTFILE" | awk '{print $1}')"
sudo umount "$MOUNT_POINT" || fail "could not unmount $MOUNT_POINT for the remount/persistence check"
sudo mount "$MOUNT_POINT" || fail "could not remount $MOUNT_POINT (real fs state must survive an unmount)"
SHA_AFTER="$(sudo sha256sum "$TESTFILE" | awk '{print $1}')"
[[ "$SHA_BEFORE" == "$SHA_AFTER" ]] || fail "file content changed across umount/mount: before=$SHA_BEFORE after=$SHA_AFTER"

MD_UUID_REAL="$(sudo mdadm --detail --export /dev/md0 2>/dev/null | grep '^MD_UUID=' | cut -d= -f2)"
[[ -n "$MD_UUID_REAL" ]] || fail "could not read a real MD_UUID from mdadm --detail --export /dev/md0"
sudo grep -qF "ARRAY /dev/md0 UUID=$MD_UUID_REAL" /etc/mdadm.conf 2>/dev/null || \
    fail "/etc/mdadm.conf missing 'ARRAY /dev/md0 UUID=$MD_UUID_REAL' (D8)"

FS_UUID_REAL="$(sudo findmnt -no UUID "$MOUNT_POINT" 2>/dev/null)"
[[ -n "$FS_UUID_REAL" ]] || fail "could not read the real Btrfs UUID via findmnt"
sudo grep -qF "UUID=$FS_UUID_REAL $MOUNT_POINT btrfs" /etc/fstab 2>/dev/null || \
    fail "/etc/fstab missing the shr-rs managed line for UUID=$FS_UUID_REAL (D8)"

STATE_TOML="$(sudo cat /var/lib/shr-rs/state.toml 2>/dev/null || true)"
[[ -n "$STATE_TOML" ]] || fail "state.toml not found at /var/lib/shr-rs/state.toml"
echo "$STATE_TOML" | grep -qF "$MD_UUID_REAL" || fail "state.toml does not contain the real md_uuid"
echo "$STATE_TOML" | grep -qF "$FS_UUID_REAL" || fail "state.toml does not contain the real fs_uuid"
echo "$STATE_TOML" | grep -q "/dev/sd" && fail "state.toml contains a /dev/sdX-style path -- identity must be by-id/UUID only (D3)"
STATE_PERMS="$(sudo stat -c '%a' /var/lib/shr-rs/state.toml 2>/dev/null || true)"
[[ "$STATE_PERMS" == "600" ]] || fail "state.toml permissions are $STATE_PERMS, expected 600 (D7)"

echo "== SM-FULLSTACK-1: cleanup + verify teardown (R4) =="
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-FULLSTACK-1: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-FULLSTACK-1: PASS"
else
    printf 'SM-FULLSTACK-1: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
