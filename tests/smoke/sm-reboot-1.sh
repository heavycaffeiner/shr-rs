#!/usr/bin/env bash
# SM-REBOOT-1: guest reboot survival -- the array auto-reassembles, the
# filesystem auto-mounts, and `shr-rs status` reports the same layout
# afterward, with no shr-rs process running at boot. This is the one test
# that actually proves D7/D8 (the /etc/mdadm.conf and /etc/fstab writers)
# do what they exist for.
#
# A real reboot cannot be scripted as one continuous SSH session (the
# connection dies with the guest); this script therefore runs in two
# explicit phases, with the actual `reboot` issued by the host orchestrator
# in between:
#   ./sm-reboot-1.sh before   -- create a real array, arrange for it to
#                                 survive the reboot, record its identity
#   [ host runs: gssh.ps1 "sudo reboot", waits for the guest to come back ]
#   ./sm-reboot-1.sh after    -- verify auto-reassembly/mount, verify
#                                 `shr-rs status`, clean up
#
# Loop-backed disks are this project's own test fixture, not something a
# real deployment would ever have to work around: real disks are always
# physically present at boot, but a `losetup` attachment never survives a
# reboot even though its backing file does (this guest's /tmp is real XFS,
# confirmed, not tmpfs). `before` installs a small boot-time systemd oneshot
# (lib/shr-smoke-loop-setup.sh/.service) that ONLY re-attaches the loop
# devices; everything after that -- `mdadm --assemble --scan` reading the
# real /etc/mdadm.conf, `mount -a` reading the real /etc/fstab -- is the
# actual mechanism being tested, not a simulation of it.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_POINT=/mnt/shr-smoke
STATE_FILE="$SMOKE_DIR/reboot-state.env"
UNIT_NAME=shr-smoke-loop-setup.service
RESULT=PASS
FAILURES=()

fail() {
    RESULT=FAIL
    FAILURES+=("$1")
    echo "FAIL: $1" >&2
}

phase="${1:-}"
if [[ "$phase" != "before" && "$phase" != "after" ]]; then
    echo "usage: $0 before|after" >&2
    exit 2
fi

if [[ "$phase" == "before" ]]; then
    echo "== SM-REBOOT-1 (before): arrange =="
    if ! fixture_up 16G 16G 16G; then
        echo "SM-REBOOT-1: BLOCKED (fixture_up failed)"
        exit 2
    fi
    DISKS="ata-LOOP_DISK_10,ata-LOOP_DISK_11,ata-LOOP_DISK_12"

    OUTPUT="$(sudo "$SHR_RS" --json create --mode shr --disks "$DISKS" --mount "$MOUNT_POINT" --compression zstd:3 2>&1)"
    CREATE_EXIT=$?
    echo "$OUTPUT"
    if [[ "$CREATE_EXIT" -ne 0 ]]; then
        echo "SM-REBOOT-1: BLOCKED (create failed, nothing to reboot-test)"
        fixture_down >/dev/null 2>&1 || true
        exit 2
    fi

    echo "== SM-REBOOT-1 (before): install boot-time loop reattachment =="
    sudo install -m 755 lib/shr-smoke-loop-setup.sh /usr/local/bin/shr-smoke-loop-setup.sh
    sudo install -m 644 lib/shr-smoke-loop-setup.service "/etc/systemd/system/$UNIT_NAME"
    sudo systemctl daemon-reload
    sudo systemctl enable "$UNIT_NAME"

    echo "== SM-REBOOT-1 (before): wait for initial resync to finish =="
    # This test exists to prove steady-state reboot survival (D7/D8), not
    # boot-time behavior while the array is still building itself -- and
    # rebooting mid-resync is exactly what previously made boot-time
    # `mdadm --assemble --scan` slow enough (on this TCG-emulated guest) to
    # hit systemd's default 90s start timeout, which killed
    # shr-smoke-loop-setup.service before it ever reached
    # `vgchange -ay`/`mount -a` (see lib/shr-smoke-loop-setup.service). Poll
    # every 10s, up to 90 attempts (~15 min): a 16G-member RAID5 initial
    # resync takes several minutes on this guest.
    RESYNC_WAIT=0
    RESYNC_ATTEMPTS=90
    RESYNC_SETTLED=0
    while [[ "$RESYNC_WAIT" -lt "$RESYNC_ATTEMPTS" ]]; do
        SYNC_ACTION="$(cat /sys/block/md0/md/sync_action 2>/dev/null || echo unknown)"
        if ! grep -Eq 'recovery|resync|reshape' /proc/mdstat && [[ "$SYNC_ACTION" == "idle" ]]; then
            RESYNC_SETTLED=1
            break
        fi
        if (( RESYNC_WAIT % 6 == 0 )); then
            echo "SM-REBOOT-1: waiting for initial resync to finish (sync_action=$SYNC_ACTION)..."
            grep -E 'recovery|resync|reshape' /proc/mdstat || true
        fi
        sleep 10
        RESYNC_WAIT=$((RESYNC_WAIT + 1))
    done
    if [[ "$RESYNC_SETTLED" -ne 1 ]]; then
        echo "SM-REBOOT-1: BLOCKED (initial resync did not finish in time)"
        sudo systemctl disable "$UNIT_NAME" 2>/dev/null || true
        sudo rm -f "/etc/systemd/system/$UNIT_NAME" /usr/local/bin/shr-smoke-loop-setup.sh
        sudo systemctl daemon-reload
        sudo umount "$MOUNT_POINT" 2>/dev/null || true
        fixture_down >/dev/null 2>&1 || true
        exit 2
    fi

    MD_UUID="$(sudo mdadm --detail --export /dev/md0 | grep '^MD_UUID=' | cut -d= -f2)"
    FS_UUID="$(sudo findmnt -no UUID "$MOUNT_POINT")"
    if [[ -z "$MD_UUID" || -z "$FS_UUID" ]]; then
        echo "SM-REBOOT-1: BLOCKED (could not read real md_uuid/fs_uuid before reboot)"
        sudo systemctl disable "$UNIT_NAME" 2>/dev/null || true
        sudo rm -f "/etc/systemd/system/$UNIT_NAME" /usr/local/bin/shr-smoke-loop-setup.sh
        sudo systemctl daemon-reload
        sudo umount "$MOUNT_POINT" 2>/dev/null || true
        fixture_down >/dev/null 2>&1 || true
        exit 2
    fi

    TESTFILE="$MOUNT_POINT/reboot-survival-test.bin"
    # Unlike every other write in this phase (create, uuid reads above), this
    # block was previously unchecked: a silent dd/sync/sha256sum failure
    # still printed "BEFORE phase complete" and asked for a reboot, wasting a
    # full reboot round trip before the `after` phase's file-missing check
    # finally caught it. Fail fast here instead, same BLOCKED+cleanup+exit 2
    # shape as the MD_UUID/FS_UUID check just above.
    if ! sudo dd if=/dev/urandom of="$TESTFILE" bs=1M count=4 status=none; then
        echo "SM-REBOOT-1: BLOCKED (could not write test file before reboot)"
        sudo systemctl disable "$UNIT_NAME" 2>/dev/null || true
        sudo rm -f "/etc/systemd/system/$UNIT_NAME" /usr/local/bin/shr-smoke-loop-setup.sh
        sudo systemctl daemon-reload
        sudo umount "$MOUNT_POINT" 2>/dev/null || true
        fixture_down >/dev/null 2>&1 || true
        exit 2
    fi
    TESTFILE_SIZE="$(sudo stat -c%s "$TESTFILE" 2>/dev/null || echo 0)"
    if [[ "$TESTFILE_SIZE" != "4194304" ]]; then
        echo "SM-REBOOT-1: BLOCKED (test file is $TESTFILE_SIZE bytes, expected 4194304 -- dd truncated?)"
        sudo systemctl disable "$UNIT_NAME" 2>/dev/null || true
        sudo rm -f "/etc/systemd/system/$UNIT_NAME" /usr/local/bin/shr-smoke-loop-setup.sh
        sudo systemctl daemon-reload
        sudo umount "$MOUNT_POINT" 2>/dev/null || true
        fixture_down >/dev/null 2>&1 || true
        exit 2
    fi
    if ! sudo sync; then
        echo "SM-REBOOT-1: BLOCKED (sync failed -- test file write not durable before reboot)"
        sudo systemctl disable "$UNIT_NAME" 2>/dev/null || true
        sudo rm -f "/etc/systemd/system/$UNIT_NAME" /usr/local/bin/shr-smoke-loop-setup.sh
        sudo systemctl daemon-reload
        sudo umount "$MOUNT_POINT" 2>/dev/null || true
        fixture_down >/dev/null 2>&1 || true
        exit 2
    fi
    SHA_BEFORE="$(sudo sha256sum "$TESTFILE" | awk '{print $1}')"
    if [[ -z "$SHA_BEFORE" ]]; then
        echo "SM-REBOOT-1: BLOCKED (could not compute sha256 of test file before reboot)"
        sudo systemctl disable "$UNIT_NAME" 2>/dev/null || true
        sudo rm -f "/etc/systemd/system/$UNIT_NAME" /usr/local/bin/shr-smoke-loop-setup.sh
        sudo systemctl daemon-reload
        sudo umount "$MOUNT_POINT" 2>/dev/null || true
        fixture_down >/dev/null 2>&1 || true
        exit 2
    fi

    if ! {
        echo "MD_UUID=$MD_UUID"
        echo "FS_UUID=$FS_UUID"
        echo "SHA_BEFORE=$SHA_BEFORE"
        echo "MOUNT_POINT=$MOUNT_POINT"
    } | sudo tee "$STATE_FILE" >/dev/null; then
        echo "SM-REBOOT-1: BLOCKED (could not persist reboot-state file $STATE_FILE)"
        sudo systemctl disable "$UNIT_NAME" 2>/dev/null || true
        sudo rm -f "/etc/systemd/system/$UNIT_NAME" /usr/local/bin/shr-smoke-loop-setup.sh
        sudo systemctl daemon-reload
        sudo umount "$MOUNT_POINT" 2>/dev/null || true
        fixture_down >/dev/null 2>&1 || true
        exit 2
    fi

    echo "== SM-REBOOT-1 (before): recorded =="
    cat "$STATE_FILE"
    echo "SM-REBOOT-1: BEFORE phase complete -- reboot the guest now, then run '$0 after'"
    exit 0
fi

# phase == after
echo "== SM-REBOOT-1 (after): verify =="
if [[ ! -f "$STATE_FILE" ]]; then
    echo "SM-REBOOT-1: BLOCKED (no $STATE_FILE -- was 'before' run and did the guest actually reboot?)"
    exit 2
fi
# shellcheck source=/dev/null
source "$STATE_FILE"

MDSTAT="$(cat /proc/mdstat)"
echo "$MDSTAT"
echo "$MDSTAT" | grep -q "^md0" || fail "md0 did not auto-reassemble after reboot"

REAL_MD_UUID="$(sudo mdadm --detail --export /dev/md0 2>/dev/null | grep '^MD_UUID=' | cut -d= -f2)"
[[ "$REAL_MD_UUID" == "$MD_UUID" ]] || fail "reassembled array's UUID differs: before=$MD_UUID after=$REAL_MD_UUID"

FINDMNT="$(findmnt -no FSTYPE,UUID "$MOUNT_POINT" 2>/dev/null || true)"
echo "findmnt: $FINDMNT"
echo "$FINDMNT" | grep -q "btrfs" || fail "$MOUNT_POINT did not auto-mount as btrfs after reboot"
echo "$FINDMNT" | grep -qF "$FS_UUID" || fail "mounted filesystem UUID differs from before reboot (findmnt: $FINDMNT, expected $FS_UUID)"

TESTFILE="$MOUNT_POINT/reboot-survival-test.bin"
if [[ -f "$TESTFILE" ]]; then
    SHA_AFTER="$(sudo sha256sum "$TESTFILE" | awk '{print $1}')"
    [[ "$SHA_AFTER" == "$SHA_BEFORE" ]] || fail "test file content changed across reboot: before=$SHA_BEFORE after=$SHA_AFTER"
else
    fail "test file written before reboot is missing after reboot"
fi

STATUS_JSON="$(sudo "$SHR_RS" --json status 2>&1)"
echo "$STATUS_JSON"
echo "$STATUS_JSON" | grep -q '"raid5"' || fail "shr-rs status does not report the raid5 array after reboot"

echo "== SM-REBOOT-1 (after): cleanup + verify teardown (R4) =="
sudo systemctl disable "$UNIT_NAME" 2>/dev/null || true
sudo rm -f "/etc/systemd/system/$UNIT_NAME" /usr/local/bin/shr-smoke-loop-setup.sh
sudo systemctl daemon-reload
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-REBOOT-1: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-REBOOT-1: PASS"
else
    printf 'SM-REBOOT-1: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
