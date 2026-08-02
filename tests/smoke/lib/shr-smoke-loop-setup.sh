#!/usr/bin/env bash
# SM-REBOOT-1 fixture helper, installed as a boot-time systemd oneshot on
# the guest ONLY for the duration of that test.
#
# Real disks are always physically present at boot; this project's loop-
# device-backed smoke fixture is the one thing that ISN'T -- `losetup`
# attachments never survive a reboot even though the backing image files
# under /tmp (real XFS on this guest, not tmpfs) do. This script exists
# solely to re-attach those loop devices so the REST of the boot sequence
# -- `mdadm --assemble --scan` reading the real /etc/mdadm.conf, `mount -a`
# reading the real /etc/fstab -- is exercised exactly as it would be for
# real disks. It is the only fixture-specific concession in SM-REBOOT-1;
# everything after the `losetup` calls below is the actual thing being
# tested (D7/D8), not a simulation of it.
set -x

# Must track lib/fixture.sh's SMOKE_DIR default and reserved loop band --
# this script re-attaches whatever fixture_up actually created, so a stale
# default here (after fixture.sh's default moved to the guest's dedicated
# /srv/shr-smoke disk) would silently find nothing to reattach.
SMOKE_DIR="${SMOKE_DIR:-/srv/shr-smoke}"

for i in $(seq 10 29); do
    img="$SMOKE_DIR/disk_${i}.img"
    [[ -f "$img" ]] || continue
    losetup "/dev/loop${i}" &>/dev/null && continue
    # -P is required: this guest has /sys/module/loop/parameters/max_part=0,
    # so a plain `losetup` does NOT scan the image's partition table and
    # /dev/loop${i}p1 never gets registered with the kernel (confirmed via
    # /proc/partitions + /sys/class/block on the real guest). Without -P,
    # everything downstream (mdadm --examine, --assemble --scan) silently
    # has no partition device to find.
    losetup -P "/dev/loop${i}" "$img"
    ln -sf "/dev/loop${i}" "/dev/disk/by-id/ata-LOOP_DISK_${i}"
done

udevadm settle --timeout=10 2>/dev/null || true
# Real disks trigger this via udev incremental assembly as each member
# appears; loop devices attached by a script after udev has already
# settled once do not reliably re-trigger those rules, so invoke the same
# mdadm.conf-driven assembly directly.
#
# stderr is NOT suppressed below: it previously was (`2>/dev/null`), which
# hid the real "no such device" errors caused by the max_part=0 issue above
# and made this failure take far longer to diagnose than it should have.
# `|| true` still guarantees a failure here can never block the guest's
# boot; it just no longer does so silently.
mdadm --assemble --scan || true
udevadm settle --timeout=10 2>/dev/null || true
vgscan || true
vgchange -ay || true
# Ensure the device-mapper node for the logical volume actually exists
# before mount -a goes looking for it.
udevadm settle --timeout=10 || true
mount -a || true

exit 0
