#!/usr/bin/env bash
# SM-GROUPS-4: four independent SHR groups coexisting on one host at once --
# {shr, shr2} x {uniform, heterogeneous} -- the multi-group demo target.
# This is also the regression test for the host-wide `/dev/mdN` allocation
# fix: `used_md_numbers` alone only sees bands already recorded in
# state.toml, so a bug there would let a later group's `mdadm --create`
# collide with an earlier group's still-live array (or a foreign one on the
# host). Every group here is created with a REAL `shr-rs create` against the
# guest's mdadm/LVM/Btrfs stack -- no dry-run -- and every assertion below
# reads real kernel/filesystem state, never shr-rs's own exit code or
# printed message.
#
# Layout (planner arithmetic: reserved_head=128MiB, band_alignment=4GiB, so
# usable(size) = align_down(size - 128MiB, 4GiB) -- usable(8G)=4G,
# usable(12G)=8G):
#
#   group         mode  disks (GiB)         bands
#   shr-uniform   shr   8,8,8               band0: 3 members raid5
#   shr-hetero    shr   8,12,12             band0: 3 members raid5
#                                           band1: 2 members raid1 (12G disks' extra 4G)
#   shr2-uniform  shr2  8,8,8,8             band0: 4 members raid6
#   shr2-hetero   shr2  8,12,12,12,12       band0: 5 members raid6
#                                           band1: 4 members raid6 (12G disks' extra 4G)
#
# 15 disks total: ata-LOOP_DISK_10 .. ata-LOOP_DISK_24, assigned to groups in
# the order fixture_up hands them out (see DISKS below).
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

# NOTE: do NOT rename this to GROUPS. `GROUPS` is a bash special variable
# that holds the numeric group IDs (GIDs) of the current user -- bash
# silently ignores any assignment to it and keeps the real GIDs instead, so
# a `GROUPS=(...)` here would leave the array holding the invoking user's
# GIDs (e.g. 1000, 10) rather than the group names below, and every loop
# over it would then blow up deep in the script with a confusing
# `unbound variable` on `${MODE[$g]}` etc. instead of an obvious failure
# here. Keep the SHR_ prefix.
SHR_GROUPS=(shr-uniform shr-hetero shr2-uniform shr2-hetero)

# Cheap sanity check that the assignment above actually took effect (see the
# NOTE above -- this is exactly the class of bug where a silently-ignored
# assignment would otherwise only surface as an unrelated unbound-variable
# error much later in the run).
if [[ "${#SHR_GROUPS[@]}" -ne 4 || "${SHR_GROUPS[0]}" != "shr-uniform" ]]; then
    echo "SM-GROUPS-4: BLOCKED (SHR_GROUPS did not get the expected values -- got: ${SHR_GROUPS[*]:-<empty>})" >&2
    exit 2
fi

declare -A MODE=(
    [shr-uniform]=shr
    [shr-hetero]=shr
    [shr2-uniform]=shr2
    [shr2-hetero]=shr2
)
declare -A DISKS=(
    [shr-uniform]="ata-LOOP_DISK_10,ata-LOOP_DISK_11,ata-LOOP_DISK_12"
    [shr-hetero]="ata-LOOP_DISK_13,ata-LOOP_DISK_14,ata-LOOP_DISK_15"
    [shr2-uniform]="ata-LOOP_DISK_16,ata-LOOP_DISK_17,ata-LOOP_DISK_18,ata-LOOP_DISK_19"
    [shr2-hetero]="ata-LOOP_DISK_20,ata-LOOP_DISK_21,ata-LOOP_DISK_22,ata-LOOP_DISK_23,ata-LOOP_DISK_24"
)
declare -A VG=(
    [shr-uniform]=shr_vg_uniform
    [shr-hetero]=shr_vg_hetero
    [shr2-uniform]=shr2_vg_uniform
    [shr2-hetero]=shr2_vg_hetero
)
declare -A LV=(
    [shr-uniform]=data_uniform
    [shr-hetero]=data_hetero
    [shr2-uniform]=data_uniform
    [shr2-hetero]=data_hetero
)
declare -A MOUNT=(
    [shr-uniform]=/mnt/shr-uniform
    [shr-hetero]=/mnt/shr-hetero
    [shr2-uniform]=/mnt/shr2-uniform
    [shr2-hetero]=/mnt/shr2-hetero
)

# Every real md device name this test observes, across all 6 expected bands
# of all 4 groups -- populated by check_band below, then checked for
# cross-group uniqueness (the multi-group collision trap this test exists
# to catch) once every group has been created.
ALL_MD_NAMES=()

cleanup() {
    for g in "${SHR_GROUPS[@]}"; do
        sudo umount "${MOUNT[$g]}" 2>/dev/null || true
    done
    fixture_down >/dev/null 2>&1 || true
}
trap cleanup EXIT

# find_md_for_members MEMBER...: echo the md device name (e.g. "md0") whose
# /proc/mdstat line contains every given member partition token, or nothing
# if no such line exists. Independent of shr-rs's own bookkeeping -- this
# only reads the kernel's own array table, keyed by which physical
# partitions the planner arithmetic above says a given band MUST contain.
find_md_for_members() {
    local line
    while IFS= read -r line; do
        local ok=1
        local member
        for member in "$@"; do
            case "$line" in
                *"${member}["*) ;;
                *) ok=0; break ;;
            esac
        done
        if [[ "$ok" == 1 ]]; then
            echo "${line%% *}"
            return 0
        fi
    done < <(grep -E '^md[0-9]+ :' /proc/mdstat)
    return 1
}

# check_band LABEL EXPECTED_LEVEL EXPECTED_COUNT MEMBER...: poll
# /proc/mdstat (bounded, printing progress -- the guest is TCG-emulated and
# creating 4 groups back-to-back means real resyncs stacking up) until an
# array with exactly these member partitions shows up, then assert its
# level and member count independently of anything shr-rs reported. Appends
# the resolved md name to ALL_MD_NAMES for the cross-group uniqueness check.
check_band() {
    local label="$1" expected_level="$2" expected_count="$3"
    shift 3
    local members=("$@")
    local attempts=0 max_attempts=24 # 24 * 5s = 2 minutes
    local md_name="" line=""
    while (( attempts < max_attempts )); do
        md_name="$(find_md_for_members "${members[@]}")"
        if [[ -n "$md_name" ]]; then
            line="$(grep "^${md_name} :" /proc/mdstat)"
            break
        fi
        if (( attempts % 6 == 0 )); then
            echo "SM-GROUPS-4: waiting for $label to appear in /proc/mdstat (members: ${members[*]})..."
        fi
        sleep 5
        attempts=$((attempts + 1))
    done
    if [[ -z "$md_name" ]]; then
        fail "$label: no md array ever appeared with members ${members[*]}"
        return
    fi
    echo "$label -> /dev/$md_name : $line"
    echo "$line" | grep -q "$expected_level" || fail "$label ($md_name): expected level $expected_level, got: $line"
    local actual_count
    actual_count="$(echo "$line" | grep -oE 'loop[0-9]+p[0-9]+\[[0-9]+\]' | wc -l)"
    [[ "$actual_count" == "$expected_count" ]] || \
        fail "$label ($md_name): expected exactly $expected_count members, got $actual_count: $line"
    ALL_MD_NAMES+=("$md_name")
}

echo "== SM-GROUPS-4: arrange =="
if ! fixture_up 8G 8G 8G 8G 12G 12G 8G 8G 8G 8G 8G 12G 12G 12G 12G; then
    echo "SM-GROUPS-4: BLOCKED (fixture_up failed)"
    exit 2
fi

echo "== SM-GROUPS-4: act (create all 4 groups, sequentially -- shr-rs's own"
echo "   state lock refuses concurrent create/expand invocations, and LVM"
echo "   metadata operations are not safe to race against each other anyway) =="
for g in "${SHR_GROUPS[@]}"; do
    echo "-- creating group $g (mode=${MODE[$g]}, disks=${DISKS[$g]}) --"
    OUTPUT="$(sudo "$SHR_RS" --json create --mode "${MODE[$g]}" --disks "${DISKS[$g]}" \
        --name "$g" --vg-name "${VG[$g]}" --lv-name "${LV[$g]}" \
        --mount "${MOUNT[$g]}" --compression zstd:3 2>&1)"
    CREATE_EXIT=$?
    echo "$OUTPUT"
    echo "create $g exit code: $CREATE_EXIT"
    if [[ "$CREATE_EXIT" -ne 0 ]]; then
        echo "SM-GROUPS-4: BLOCKED (create failed for $g)"
        cat /proc/mdstat
        exit 2
    fi
done

echo "== SM-GROUPS-4: assert (independent kernel/filesystem state observation) =="

echo "-- band membership, level, and member count (kernel-derived, not shr-rs-derived) --"
check_band "shr-uniform band0" raid5 3 loop10p1 loop11p1 loop12p1
check_band "shr-hetero band0" raid5 3 loop13p1 loop14p1 loop15p1
check_band "shr-hetero band1" raid1 2 loop14p2 loop15p2
check_band "shr2-uniform band0" raid6 4 loop16p1 loop17p1 loop18p1 loop19p1
check_band "shr2-hetero band0" raid6 5 loop20p1 loop21p1 loop22p1 loop23p1 loop24p1
check_band "shr2-hetero band1" raid6 4 loop21p2 loop22p2 loop23p2 loop24p2

echo "-- no two groups share an md device name (the multi-group collision trap) --"
if [[ "${#ALL_MD_NAMES[@]}" -eq 0 ]]; then
    # Every check_band above already failed and recorded its own fail() --
    # nothing to cross-check, and an empty-array `printf ... | wc -l` would
    # misreport 1 distinct name instead of 0 here, a false signal on top of
    # already-recorded real failures.
    echo "SKIPPED: no md arrays were resolved above (see failures already recorded)"
else
    UNIQUE_MD_COUNT="$(printf '%s\n' "${ALL_MD_NAMES[@]}" | sort -u | wc -l)"
    if [[ "$UNIQUE_MD_COUNT" != "${#ALL_MD_NAMES[@]}" ]]; then
        fail "duplicate /dev/mdN name reused across bands/groups: ${ALL_MD_NAMES[*]} (${#ALL_MD_NAMES[@]} bands, only $UNIQUE_MD_COUNT distinct names)"
    else
        echo "OK: ${#ALL_MD_NAMES[@]} bands, ${UNIQUE_MD_COUNT} distinct md device names: ${ALL_MD_NAMES[*]}"
    fi
fi

echo "-- all 4 filesystems mounted, btrfs, compress=zstd:3 (findmnt) --"
for g in "${SHR_GROUPS[@]}"; do
    FINDMNT="$(findmnt -no FSTYPE,OPTIONS "${MOUNT[$g]}" 2>/dev/null || true)"
    echo "$g findmnt: $FINDMNT"
    echo "$FINDMNT" | grep -q "btrfs" || fail "$g: ${MOUNT[$g]} is not mounted as btrfs (findmnt: $FINDMNT)"
    echo "$FINDMNT" | grep -q "compress=zstd:3" || fail "$g: ${MOUNT[$g]} missing compress=zstd:3 mount option (findmnt: $FINDMNT)"
done

echo "-- write + sha256sum readback across an umount/mount cycle, per group --"
for g in "${SHR_GROUPS[@]}"; do
    TESTFILE="${MOUNT[$g]}/smoke-write-test.bin"
    sudo dd if=/dev/urandom of="$TESTFILE" bs=1M count=4 status=none || fail "$g: could not write test file to ${MOUNT[$g]}"
    sudo sync
    SHA_BEFORE="$(sudo sha256sum "$TESTFILE" | awk '{print $1}')"
    sudo umount "${MOUNT[$g]}" || fail "$g: could not unmount ${MOUNT[$g]} for the remount/persistence check"
    sudo mount "${MOUNT[$g]}" || fail "$g: could not remount ${MOUNT[$g]} (real fs state must survive an unmount)"
    SHA_AFTER="$(sudo sha256sum "$TESTFILE" | awk '{print $1}')"
    [[ "$SHA_BEFORE" == "$SHA_AFTER" ]] || \
        fail "$g: file content changed across umount/mount: before=$SHA_BEFORE after=$SHA_AFTER"
done

echo "-- /etc/mdadm.conf has an ARRAY line for every band of every group (regression:"
echo "   creating group N must never wipe group N-1's entries) --"
MDADM_CONF="$(sudo cat /etc/mdadm.conf 2>/dev/null || true)"
for md_name in "${ALL_MD_NAMES[@]}"; do
    MD_UUID_REAL="$(sudo mdadm --detail --export "/dev/$md_name" 2>/dev/null | grep '^MD_UUID=' | cut -d= -f2)"
    if [[ -z "$MD_UUID_REAL" ]]; then
        fail "could not read a real MD_UUID from mdadm --detail --export /dev/$md_name"
        continue
    fi
    echo "$MDADM_CONF" | grep -qF "ARRAY /dev/$md_name UUID=$MD_UUID_REAL" || \
        fail "/etc/mdadm.conf missing 'ARRAY /dev/$md_name UUID=$MD_UUID_REAL'"
done

echo "-- /etc/fstab has a managed line for every group, keyed by the real Btrfs UUID,"
echo "   with no /dev/sdX-style path anywhere (D3/D8) --"
FSTAB="$(sudo cat /etc/fstab 2>/dev/null || true)"
for g in "${SHR_GROUPS[@]}"; do
    FS_UUID_REAL="$(sudo findmnt -no UUID "${MOUNT[$g]}" 2>/dev/null || true)"
    if [[ -z "$FS_UUID_REAL" ]]; then
        fail "$g: could not read the real Btrfs UUID via findmnt for ${MOUNT[$g]}"
        continue
    fi
    echo "$FSTAB" | grep -qF "UUID=$FS_UUID_REAL ${MOUNT[$g]} btrfs" || \
        fail "$g: /etc/fstab missing the shr-rs managed line for UUID=$FS_UUID_REAL ${MOUNT[$g]}"
done
echo "$FSTAB" | grep -Eq '/dev/sd[a-z]' && fail "/etc/fstab contains a /dev/sdX-style path -- identity must be by-id/UUID only (D3)"

echo "-- shr-rs --json groups reports exactly 4 groups with the expected names/modes --"
GROUPS_JSON="$(sudo "$SHR_RS" --json groups 2>&1)"
echo "$GROUPS_JSON"
GROUP_COUNT="$(echo "$GROUPS_JSON" | grep -Fc '"name":')"
[[ "$GROUP_COUNT" == 4 ]] || fail "shr-rs --json groups reported $GROUP_COUNT groups, expected exactly 4"
for g in "${SHR_GROUPS[@]}"; do
    echo "$GROUPS_JSON" | grep -Fq "\"name\": \"$g\"" || fail "shr-rs --json groups is missing group '$g'"
done
MODE_SHR_COUNT="$(echo "$GROUPS_JSON" | grep -Fc '"mode": "shr"')"
MODE_SHR2_COUNT="$(echo "$GROUPS_JSON" | grep -Fc '"mode": "shr2"')"
[[ "$MODE_SHR_COUNT" == 2 ]] || fail "expected 2 groups reporting mode=shr, got $MODE_SHR_COUNT"
[[ "$MODE_SHR2_COUNT" == 2 ]] || fail "expected 2 groups reporting mode=shr2, got $MODE_SHR2_COUNT"

echo "== SM-GROUPS-4: cleanup + verify teardown (R4) =="
for g in "${SHR_GROUPS[@]}"; do
    sudo umount "${MOUNT[$g]}" 2>/dev/null || true
done
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-GROUPS-4: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-GROUPS-4: PASS"
else
    printf 'SM-GROUPS-4: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
