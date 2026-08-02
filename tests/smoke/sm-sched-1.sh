#!/usr/bin/env bash
# after `shr-rs schedule install`, the timer units actually exist
# in systemd (`systemctl list-timers`, `systemctl is-enabled`), and the
# generated unit files carry the real, resolved `shr-rs` binary path in their
# `ExecStart=` line (the whole reason this ever became a bug class).
#
# A note on the marker, and on how this script's own premise changed:
# The design's SM-SCHED-1 row says the generated units must sit "inside
# the managed marker block". When this script was first
# written that was not true of unit files -- the marker
# (`# >>> shr-rs managed >>>`) existed only to splice /etc/mdadm.conf and
# /etc/fstab, and *.service/*.timer files were plain-overwritten with no
# marker text at all, so asserting one would have been asserting something
# the shipped code deliberately never did.
#
# Changed that, and for a reason worth stating: a `destroy`d group's
# timer used to stay enabled forever with nothing able to tell it apart from
# an operator's own hand-written unit of the same name. So every generated
# unit now carries that same marker as a leading comment line (systemd
# ignores `#` lines), and `destroy`/`schedule install --prune` use it as the
# ownership test -- deleting only what carries it and merely warning about
# lookalikes that do not. The marker is now load-bearing, so this script
# asserts it. Verified on the guest: `head -1 shr-rs-scrub-ga.service`
# reads `# >>> shr-rs managed >>>`.
#
# What the marker block buys mdadm.conf/fstab -- "installing again never
# duplicates or corrupts what's already there" -- also has a real equivalent
# for unit files (plain idempotent overwrite), and this script verifies that
# too: install twice, then independently confirm (via
# `systemctl list-timers`, not shr-rs's own report) that there is still
# exactly one of each timer, correctly enabled, with correct content.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
# shellcheck source=lib/fixture.sh
source lib/fixture.sh

SHR_RS="${SHR_RS:-/tmp/shr-rs}"
MOUNT_POINT=/mnt/shr-smoke
UNIT_DIR=/etc/systemd/system
# Must match `UNIT_OWNERSHIP_MARKER` in crates/shr-state/src/conf.rs,
# which deliberately reuses `BEGIN_MARKER` rather than inventing a second
# "is this ours" concept.
OWNERSHIP_MARKER='# >>> shr-rs managed >>>'
GROUP_NAME=default
TIMER_NAMES=(shr-rs-scrub-$GROUP_NAME shr-rs-throttle-tick shr-rs-health-check)
RESULT=PASS
FAILURES=()

fail() {
    RESULT=FAIL
    FAILURES+=("$1")
    echo "FAIL: $1" >&2
}

cleanup() {
    for t in "${TIMER_NAMES[@]}"; do
        sudo systemctl disable --now "$t.timer" 2>/dev/null || true
        sudo rm -f "$UNIT_DIR/$t.timer" "$UNIT_DIR/$t.service"
    done
    sudo systemctl daemon-reload 2>/dev/null || true
    sudo umount "$MOUNT_POINT" 2>/dev/null || true
    fixture_down >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo "== SM-SCHED-1: arrange (a real single-group array; schedule install"
echo "   needs at least one group with bands to have anything to write) =="
if ! fixture_up 8G 8G 8G; then
    echo "SM-SCHED-1: BLOCKED (fixture_up failed)"
    exit 2
fi
DISKS="ata-LOOP_DISK_10,ata-LOOP_DISK_11,ata-LOOP_DISK_12"
CREATE_OUTPUT="$(sudo "$SHR_RS" --json create --mode shr --disks "$DISKS" --mount "$MOUNT_POINT" 2>&1)"
CREATE_EXIT=$?
echo "$CREATE_OUTPUT"
if [[ "$CREATE_EXIT" -ne 0 ]]; then
    echo "SM-SCHED-1: BLOCKED (create failed)"
    fixture_down >/dev/null 2>&1 || true
    exit 2
fi

REAL_SHR_RS_PATH="$(readlink -f "$(command -v "$SHR_RS")")"
echo "resolved shr-rs binary path (what ExecStart= must embed): $REAL_SHR_RS_PATH"

echo "== SM-SCHED-1: act (schedule install) =="
INSTALL_OUTPUT="$(sudo "$SHR_RS" --json schedule install 2>&1)"
INSTALL_EXIT=$?
echo "$INSTALL_OUTPUT"
echo "schedule install exit code: $INSTALL_EXIT"
[[ "$INSTALL_EXIT" -eq 0 ]] || fail "schedule install should have succeeded"

echo "== SM-SCHED-1: assert (independent systemd/filesystem state observation) =="

LIST_TIMERS="$(systemctl list-timers --all --no-legend 2>/dev/null || true)"
echo "$LIST_TIMERS"
for t in "${TIMER_NAMES[@]}"; do
    unit_file="$UNIT_DIR/$t.service"
    timer_file="$UNIT_DIR/$t.timer"
    [[ -f "$unit_file" ]] || fail "$t.service was not written to $UNIT_DIR"
    [[ -f "$timer_file" ]] || fail "$t.timer was not written to $UNIT_DIR"

    echo "$LIST_TIMERS" | grep -qF "$t.timer" || fail "$t.timer does not appear in 'systemctl list-timers --all' -- it was never actually scheduled"

    ENABLED_STATE="$(systemctl is-enabled "$t.timer" 2>/dev/null || echo unknown)"
    echo "$t.timer is-enabled: $ENABLED_STATE"
    [[ "$ENABLED_STATE" == "enabled" ]] || fail "$t.timer is not enabled (systemctl is-enabled reported '$ENABLED_STATE')"

    if [[ -f "$unit_file" ]]; then
        grep -qF "ExecStart=$REAL_SHR_RS_PATH" "$unit_file" || \
            fail "$t.service's ExecStart= does not embed the real resolved shr-rs binary path ($REAL_SHR_RS_PATH) -- got: $(grep '^ExecStart=' "$unit_file" || echo '<no ExecStart= line>')"
    fi

    # The ownership marker must be the FIRST line, not merely present
    # somewhere. `is_shr_rs_owned_unit` tests with `starts_with`, so a marker
    # buried further down would read as "not ours" and the unit would survive
    # both `destroy`'s cleanup and `schedule install`'s pruning forever --
    # exactly the orphaned-timer bug this check exists to close.
    for f in "$unit_file" "$timer_file"; do
        [[ -f "$f" ]] || continue
        FIRST_LINE="$(head -1 "$f")"
        [[ "$FIRST_LINE" == "$OWNERSHIP_MARKER" ]] || \
            fail "$(basename "$f") does not begin with the shr-rs ownership marker -- first line is: $FIRST_LINE"
    done
done

echo "-- install a second time: must not duplicate or corrupt anything"
echo "   (the real equivalent of what a marker-splice would have protected) --"
INSTALL_OUTPUT_2="$(sudo "$SHR_RS" --json schedule install 2>&1)"
INSTALL_EXIT_2=$?
echo "$INSTALL_OUTPUT_2"
[[ "$INSTALL_EXIT_2" -eq 0 ]] || fail "the second schedule install should also have succeeded"

LIST_TIMERS_2="$(systemctl list-timers --all --no-legend 2>/dev/null || true)"
for t in "${TIMER_NAMES[@]}"; do
    COUNT="$(echo "$LIST_TIMERS_2" | grep -cF "$t.timer")"
    [[ "$COUNT" == 1 ]] || fail "$t.timer appears $COUNT time(s) in 'systemctl list-timers' after installing twice, expected exactly 1 (duplication)"
    ENABLED_STATE_2="$(systemctl is-enabled "$t.timer" 2>/dev/null || echo unknown)"
    [[ "$ENABLED_STATE_2" == "enabled" ]] || fail "$t.timer is no longer enabled after the second install (got '$ENABLED_STATE_2')"
done

echo "== SM-SCHED-1: cleanup + verify teardown (R4) =="
for t in "${TIMER_NAMES[@]}"; do
    sudo systemctl disable --now "$t.timer" 2>/dev/null || true
    sudo rm -f "$UNIT_DIR/$t.timer" "$UNIT_DIR/$t.service"
done
sudo systemctl daemon-reload 2>/dev/null || true
sudo umount "$MOUNT_POINT" 2>/dev/null || true
if ! assert_clean; then
    fail "environment not clean after fixture_down"
fi

echo "== SM-SCHED-1: verdict =="
if [[ "$RESULT" == PASS ]]; then
    echo "SM-SCHED-1: PASS"
else
    printf 'SM-SCHED-1: FAIL\n'
    printf '  - %s\n' "${FAILURES[@]}"
fi

[[ "$RESULT" == PASS ]]
