#!/usr/bin/env bash
# shr-rs smoke test fixture: loopback disks + by-id links.
#
# Runs on the Rocky guest only. Reserved loopback band: /dev/loop10 ..
# /dev/loop29 ONLY (see the design -- widened from
# loop10-19 for the four-simultaneous-groups demo, which needs 15 disks).
# Never touch loop devices outside this range -- other guest subsystems
# (snapd, etc.) may use low numbers, and the guest's own root disk is
# /dev/vda, never a loop device or target for these fixtures.
set -uo pipefail

# The guest has a dedicated 120G disk mounted at /srv/shr-smoke specifically
# for this: holding four SHR groups live at once means mdadm resync really
# writes tens of GB across them, and the root filesystem only had ~21G free.
SMOKE_DIR="${SMOKE_DIR:-/srv/shr-smoke}"
LOOP_BASE=10
LOOP_MAX=29

smoke_loop_devices() {
    for i in $(seq "$LOOP_BASE" "$LOOP_MAX"); do
        if sudo losetup "/dev/loop$i" &>/dev/null; then
            echo "/dev/loop$i"
        fi
    done
}

# md devices with at least one member in the reserved loop band. Scoped
# deliberately (an earlier review finding): `fixture_down` must never stop or
# wipe an md array that isn't ours, even if one happens to exist on the
# host for an unrelated reason.
#
# The member regex must cover the WHOLE reserved band (loop10-loop29), not
# just its first ten numbers -- this is what scopes `fixture_down`'s `mdadm
# --stop` below. Under-matching here (the old `loop1[0-9]p[0-9]+`, which
# silently missed loop2x) means teardown leaves a live array behind whenever
# any of its members are loop20-loop29, and the NEXT test then starts dirty
# against an array `assert_clean` never even looks for.
smoke_md_devices() {
    grep -E '^md[0-9]+ :' /proc/mdstat 2>/dev/null | while IFS= read -r line; do
        if [[ "$line" =~ loop(1[0-9]|2[0-9])p[0-9]+ ]]; then
            echo "/dev/${line%% *}"
        fi
    done
}

# fixture_down: idempotent, best-effort, tears down in reverse dependency
# order (mount -> LV -> VG -> PV -> md -> loopback -> image files -> by-id
# links). Every step's failure is ignored so a partial prior state doesn't
# block cleanup -- the goal is "nothing shr-rs-managed left in the reserved
# band", not "every individual command succeeded". Every step is scoped to
# the reserved loop band (or to this project's own VG-name/mount-point
# naming) -- never touch state outside it.
#
# Four groups run at once (sm-groups-4.sh) means four distinct VG names and
# four distinct mount points, not one fixed pair -- widened from the
# original single-group `shr_vg` / `/mnt/shr-smoke` to also cover
# `shr_vg_*`/`shr2_vg_*` and `/mnt/shr2?-*`, while still matching only this
# project's own naming convention, never a blanket `/mnt/*` or every VG on
# the host.
fixture_down() {
    for mp in $(mount | awk '{print $3}' | grep -E '^/mnt/shr2?-' 2>/dev/null); do
        sudo umount "$mp" 2>/dev/null || true
    done

    local scoped_md
    scoped_md="$(smoke_md_devices)"

    for vg in $(sudo vgs --noheadings -o vg_name 2>/dev/null | tr -d ' '); do
        case "$vg" in
            shr_vg*|shr2_vg*|smoke_vg*)
                sudo lvremove -f "$vg" 2>/dev/null || true
                sudo vgremove -f "$vg" 2>/dev/null || true
                ;;
        esac
    done

    for pv in $(sudo pvs --noheadings -o pv_name 2>/dev/null | tr -d ' '); do
        if grep -qxF "$pv" <<<"$scoped_md"; then
            sudo pvremove -ff -y "$pv" 2>/dev/null || true
        fi
    done

    for md in $scoped_md; do
        sudo mdadm --stop "$md" 2>/dev/null || true
    done

    for dev in $(smoke_loop_devices); do
        for part in "${dev}"p*; do
            [[ -e "$part" ]] || continue
            sudo mdadm --zero-superblock --force "$part" 2>/dev/null || true
        done
        sudo mdadm --zero-superblock --force "$dev" 2>/dev/null || true
        sudo losetup -d "$dev" 2>/dev/null || true
    done

    sudo find /dev/disk/by-id -maxdepth 1 -name 'ata-LOOP_DISK_[12][0-9]' -delete 2>/dev/null || true
    sudo rm -rf "$SMOKE_DIR" 2>/dev/null || true
    # Legacy path used by a prior, non-versioned smoke run -- harmless
    # regular files, cleaned up for hygiene only (loop devices themselves
    # are already detached above regardless of backing-file location).
    sudo rm -f /tmp/disk_1[0-9].img 2>/dev/null || true
    sudo partprobe 2>/dev/null || true

    # Step 6 (D8) makes `create`/`expand` write real entries into the
    # guest's actual /etc/mdadm.conf and /etc/fstab. If a test array gets
    # torn down here without also stripping those entries, the NEXT guest
    # reboot (SM-REBOOT-1, or just the human working on this VM later)
    # would have systemd try to mount a UUID that no longer exists --
    # real risk of a hung/stuck boot on a fixture that's supposed to leave
    # zero trace. Delete the whole managed block (idempotent: a no-op if
    # the markers aren't present).
    sudo sed -i '/# >>> shr-rs managed >>>/,/# <<< shr-rs managed <<</d' /etc/mdadm.conf 2>/dev/null || true
    sudo sed -i '/# >>> shr-rs managed >>>/,/# <<< shr-rs managed <<</d' /etc/fstab 2>/dev/null || true
    sudo rm -f /var/lib/shr-rs/state.toml /var/lib/shr-rs/state.toml.tmp 2>/dev/null || true
    # mdadm's --grow backup file (D6) is named deterministically per md_name
    # and is never cleaned up by shr-rs itself; a fixture that stops an
    # array mid-reshape (as `fixture_down` above just did) can leave one
    # behind, which then makes mdadm refuse the NEXT test's `--grow` with
    # "cannot create backup file: File exists" (found running SM-EXPAND-1
    # twice in a row -- see the design for the production-
    # side version of this gap).
    sudo rm -f /var/lib/shr-rs/backup-*.bak 2>/dev/null || true

    return 0
}

# fixture_up SIZE... : create one loopback image per SIZE (e.g. "3G"),
# attach starting at /dev/loop10 and continuing up through /dev/loop29 (15
# disks max), and create a matching /dev/disk/by-id/ata-LOOP_DISK_<N>
# symlink for each. Idempotent: cleans up any existing reserved-band state
# first.
fixture_up() {
    assert_clean || true # tolerate a dirty start; fixture_down inside will fix it
    sudo mkdir -p "$SMOKE_DIR"
    local i=$LOOP_BASE
    for size in "$@"; do
        if (( i > LOOP_MAX )); then
            echo "fixture_up: too many disks requested (band is loop${LOOP_BASE}-loop${LOOP_MAX})" >&2
            return 1
        fi
        local img="$SMOKE_DIR/disk_${i}.img"
        sudo truncate -s "$size" "$img" || return 1
        # -P: this guest has /sys/module/loop/parameters/max_part=0, so a
        # plain `losetup` never scans the image's partition table and
        # /dev/loop${i}p1 wouldn't get registered with the kernel. This
        # currently happens to work anyway because `parted` below creates
        # the partition via a BLKPG ioctl on the already-attached device,
        # but -P is strictly more correct and keeps this attach path
        # consistent with tests/smoke/lib/shr-smoke-loop-setup.sh.
        sudo losetup -P "/dev/loop${i}" "$img" || return 1
        sudo ln -sf "/dev/loop${i}" "/dev/disk/by-id/ata-LOOP_DISK_${i}"
        i=$((i + 1))
    done
    sudo partprobe 2>/dev/null || true
}

# assert_clean: tear down, then verify the reserved band and mdadm are
# actually empty. Used both before fixture_up (don't start on a dirty
# environment) and after fixture_down (prove cleanup worked, per smoke
# rules R4 idempotency).
assert_clean() {
    fixture_down
    local leftover
    leftover="$(smoke_loop_devices)"
    if [[ -n "$leftover" ]]; then
        echo "assert_clean: loop devices still attached in reserved band: $leftover" >&2
        return 1
    fi
    local arrays
    arrays="$(smoke_md_devices)"
    if [[ -n "$arrays" ]]; then
        echo "assert_clean: md arrays with reserved-band members still present: $arrays" >&2
        return 1
    fi
    return 0
}

# smoke_find_md_for_members MEMBER...: echo the md device name (e.g. "md0")
# whose /proc/mdstat line contains every given member partition token (e.g.
# "loop10p1"), or nothing (exit 1) if no such line exists. Reads the KERNEL's
# own array table, never shr-rs's own state.toml/output -- the same
# independent-observation rule every case in this suite follows
#. Lifted out of sm-groups-4.sh's local
# `find_md_for_members` (first case that needed the same lookup from a
# second script) -- sm-groups-4.sh keeps its own copy untouched.
smoke_find_md_for_members() {
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

# smoke_wait_sync_action MD_NAME TARGET [MAX_ATTEMPTS] [SLEEP_SECS]: poll
# /sys/block/<MD_NAME>/md/sync_action until it reads exactly TARGET (e.g.
# "idle", "check", "reshape"), printing progress periodically. Returns 1 if
# it never reaches TARGET within the budget -- every caller treats that as
# BLOCKED (environment/timing issue), never as a silent pass. Defaults
# (90 attempts * 10s = 15 min) match the initial-resync wait already proven
# necessary for a 16G-member array on this TCG-emulated guest (see
# sm-reboot-1.sh's own wait loop).
smoke_wait_sync_action() {
    local md_name="$1" target="$2" max_attempts="${3:-90}" sleep_secs="${4:-10}"
    local attempt=0
    while (( attempt < max_attempts )); do
        local action
        action="$(cat "/sys/block/$md_name/md/sync_action" 2>/dev/null || echo unknown)"
        if [[ "$action" == "$target" ]]; then
            return 0
        fi
        if (( attempt % 6 == 0 )); then
            echo "waiting for $md_name/sync_action to reach '$target' (currently '$action')..." >&2
        fi
        sleep "$sleep_secs"
        attempt=$((attempt + 1))
    done
    return 1
}

# smoke_wait_sync_idle MD_NAME [MAX_ATTEMPTS] [SLEEP_SECS]: convenience
# wrapper -- the overwhelmingly common case (wait for any background
# activity, resync/recovery/reshape/check, to finish) needs no separate name.
smoke_wait_sync_idle() {
    smoke_wait_sync_action "$1" idle "${2:-90}" "${3:-10}"
}
