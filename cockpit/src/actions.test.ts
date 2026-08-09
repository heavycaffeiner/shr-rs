/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
    ExpandController,
    SimpleActionController,
    TypedConfirmController,
    buildReplaceInput,
    destroyArgs,
    expandArgs,
    expandDryRunArgs,
    expandPreflightArgs,
    filterExpandCandidates,
    filterReplacementCandidates,
    groupMemberDisks,
    hasStableId,
    isConfirmationValid,
    isValidReplacement,
    parseDestroyResult,
    parseScrubStatus,
    parseTextResult,
    reconcileArgs,
    recompressArgs,
    replaceArgs,
    scheduleInstallArgs,
    scrubCancelArgs,
    scrubStartArgs,
    scrubStatusArgs,
    snapshotCreateArgs,
    type DestroyInput,
    type ExpandInput,
    type ReplaceInput,
    type Spawn,
} from "./actions.ts";
import type { DiskStatus, GroupStatus } from "./model.ts";
import { installEnglishCatalog } from "./testCatalog.ts";

// The msgids in `src/` are dotted keys, so without a catalogue every string
// below would render as its key. See testCatalog.ts.
installEnglishCatalog();

const disk = (overrides: Partial<DiskStatus> = {}): DiskStatus => ({
    name: "sdb",
    size: 4_000_000_000_000,
    model: "Model X",
    serial: "SERIAL1",
    rotational: true,
    smart: {
        state: "ok",
        temperature_c: null,
        power_on_hours: null,
        pending_sectors: null,
        reallocated_sectors: null,
        uncorrectable_sectors: null,
        nvme_critical_warning: null,
    },
    arrays: [],
    ...overrides,
});

const okPreflightJson = JSON.stringify({
    ok: true,
    blockers: [],
    warnings: [],
    targets: [{ kernel_name: "sde", id: "ata-DISK4", size: 4_000_000_000_000, system_disk: false, system_mounts: [], has_content: false }],
});

const blockedPreflightJson = JSON.stringify({
    ok: false,
    blockers: [{ kind: "system_disk", name: "sda", id: "ata-BOOT", mounts: ["/"] }],
    warnings: [],
    targets: [{ kernel_name: "sda", id: "ata-BOOT", size: 500_000_000_000, system_disk: true, system_mounts: ["/"], has_content: true }],
});

const expandPreviewJson = JSON.stringify({
    name: "shr1",
    mode: "shr",
    layout_version: 2,
    disks: [{ id: "ata-DISK1" }, { id: "ata-DISK2" }, { id: "ata-DISK3" }, { id: "ata-DISK4" }],
    bands: [{ index: 0, level: "raid5", md_name: "md0", usable_bytes: 12_000_000_000_000, resize_pending: true }],
    filesystem: { fs_uuid: "abc-123", mount_point: "/mnt/shr_data", vg_name: "shr_vg", lv_name: "data", compression: "zstd:3" },
    planned_commands: ["mdadm --add /dev/md0 /dev/sde", "mdadm --grow /dev/md0 --raid-devices=4"],
});

const expandedJson = JSON.stringify({
    name: "shr1",
    mode: "shr",
    layout_version: 2,
    disks: [{ id: "ata-DISK1" }, { id: "ata-DISK2" }, { id: "ata-DISK3" }, { id: "ata-DISK4" }],
    bands: [{ index: 0, level: "raid5", md_name: "md0", usable_bytes: 12_000_000_000_000, resize_pending: true }],
    filesystem: { fs_uuid: "abc-123", mount_point: "/mnt/shr_data", vg_name: "shr_vg", lv_name: "data", compression: "zstd:3" },
});

const expandInput = (overrides: Partial<ExpandInput> = {}): ExpandInput => ({
    groupName: "shr1",
    diskIds: ["sde"],
    forceContent: false,
    priority: "balanced",
    ...overrides,
});

const replaceInput = (overrides: Partial<ReplaceInput> = {}): ReplaceInput => ({
    groupName: "shr1",
    oldId: "ata-DISK1",
    newId: "ata-DISK9",
    oldSize: 4_000_000_000_000,
    newSize: 4_000_000_000_000,
    ...overrides,
});

/** Records every call so tests can assert exact argv/options/order, and that
 * no call happened at all before a gate was satisfied. */
class RecordingSpawn {
    calls: { argv: string[]; options: unknown }[] = [];
    private readonly responses: (() => Promise<string>)[];

    constructor(responses: (() => Promise<string>)[]) {
        this.responses = responses;
    }

    fn: Spawn = async (argv, options) => {
        this.calls.push({ argv, options });
        const next = this.responses.shift();
        if (!next)
            throw new Error("RecordingSpawn: no more responses queued");
        return next();
    };
}

const ok = (value: string) => () => Promise.resolve(value);
const fail = (message: string) => () => Promise.reject(new Error(message));

describe("every spawn arg builder requires superuser, never try", () => {
    it("locks superuser: require across every action builder", () => {
        assert.equal(expandPreflightArgs(expandInput()).options.superuser, "require");
        assert.equal(expandDryRunArgs(expandInput()).options.superuser, "require");
        assert.equal(expandArgs(expandInput()).options.superuser, "require");
        assert.equal(replaceArgs(replaceInput()).options.superuser, "require");
        assert.equal(recompressArgs({ groupName: "shr1", compression: "zstd:3" }).options.superuser, "require");
        assert.equal(snapshotCreateArgs({ groupName: "shr1", snapshotName: "before-upgrade" }).options.superuser, "require");
        assert.equal(scrubStartArgs("shr1").options.superuser, "require");
        assert.equal(scrubStatusArgs("shr1").options.superuser, "require");
        assert.equal(scrubCancelArgs("shr1").options.superuser, "require");
        assert.equal(scheduleInstallArgs().options.superuser, "require");
        assert.equal(reconcileArgs().options.superuser, "require");
        assert.equal(destroyArgs({ groupName: "shr1", zeroSuperblocks: false }).options.superuser, "require");
    });

    it("never uses err modes other than message", () => {
        assert.equal(expandArgs(expandInput()).options.err, "message");
        assert.equal(replaceArgs(replaceInput()).options.err, "message");
        assert.equal(destroyArgs({ groupName: "shr1", zeroSuperblocks: false }).options.err, "message");
    });
});

describe("expand candidate filtering (first pass)", () => {
    it("excludes disks already claimed by any mdadm array", () => {
        const disks = [disk({ name: "sdb", arrays: ["md0"] }), disk({ name: "sde", arrays: [] })];
        const candidates = filterExpandCandidates(disks);
        assert.deepEqual(candidates.map(d => d.name), ["sde"]);
    });

    it("does not itself decide system-disk/has-content -- that is preflight's job", () => {
        // A disk with no array membership passes this filter even though it
        // might turn out to be a system disk -- only `preflight --json` (via
        // `ExpandController.runPreflight`) is authoritative for that.
        const disks = [disk({ name: "sda", arrays: [] })];
        assert.deepEqual(filterExpandCandidates(disks).map(d => d.name), ["sda"]);
    });
});

describe("expand argument builders reject bad input before spawn", () => {
    it("rejects an empty group name", () => {
        assert.throws(() => expandDryRunArgs(expandInput({ groupName: "" })));
        assert.throws(() => expandDryRunArgs(expandInput({ groupName: "   " })));
    });

    it("rejects an empty disk list", () => {
        assert.throws(() => expandDryRunArgs(expandInput({ diskIds: [] })));
        assert.throws(() => expandDryRunArgs(expandInput({ diskIds: ["  "] })));
        assert.throws(() => expandPreflightArgs(expandInput({ diskIds: [] })));
    });

    it("expandArgs is expandDryRunArgs minus --dry-run, plus --yes -- executed can't drift from previewed", () => {
        const preview = expandDryRunArgs(expandInput());
        const real = expandArgs(expandInput());
        assert.ok(preview.argv.includes("--dry-run"));
        assert.ok(!real.argv.includes("--dry-run"));
        assert.ok(real.argv.includes("--yes"));
        assert.deepEqual(real.argv.filter(a => a !== "--yes"), preview.argv.filter(a => a !== "--dry-run"));
    });

    it("--force-content only appears when explicitly requested", () => {
        assert.ok(!expandDryRunArgs(expandInput({ forceContent: false })).argv.includes("--force-content"));
        assert.ok(expandDryRunArgs(expandInput({ forceContent: true })).argv.includes("--force-content"));
    });
});

// `shr-rs expand --priority <background|balanced|max>` (shr-cli's
// PriorityArg, default_value = "balanced") was never passed through from
// Cockpit, so every Cockpit-initiated expand silently ran at the CLI's
// default regardless of what the operator wanted.
describe("expand reshape priority (--priority pass-through)", () => {
    it("balanced (the CLI's own default) produces byte-for-byte the same argv as before --priority existed -- no flag at all", () => {
        const call = expandDryRunArgs(expandInput());
        assert.deepEqual(call.argv, [
            "shr-rs", "expand",
            "--name", "shr1",
            "--add", "sde",
            "--dry-run", "--json",
        ]);
    });

    it("a non-default priority appends --priority <value>", () => {
        assert.deepEqual(expandDryRunArgs(expandInput({ priority: "background" })).argv.slice(-2), ["--priority", "background"]);
        assert.deepEqual(expandDryRunArgs(expandInput({ priority: "max" })).argv.slice(-2), ["--priority", "max"]);
    });

    it("expandArgs derivation covers a non-default priority too, not just the default case", () => {
        const input = expandInput({ priority: "max" });
        const preview = expandDryRunArgs(input);
        const real = expandArgs(input);
        assert.ok(preview.argv.includes("--priority"));
        assert.deepEqual(real.argv.filter(a => a !== "--yes"), preview.argv.filter(a => a !== "--dry-run"));
    });
});

describe("ExpandController (no execution without preview + matching confirmation)", () => {
    it("execute() throws and never spawns before any preflight ran", async () => {
        const spawn = new RecordingSpawn([]);
        const controller = new ExpandController(spawn.fn, expandInput());
        await assert.rejects(() => controller.execute());
        assert.equal(spawn.calls.length, 0);
    });

    it("a blocked preflight (system disk) prevents any dry-run/real expand spawn", async () => {
        const spawn = new RecordingSpawn([ok(blockedPreflightJson)]);
        const controller = new ExpandController(spawn.fn, expandInput({ diskIds: ["sda"] }));

        const state = await controller.runPreflight();
        assert.equal(state.step, "blocked");
        assert.equal(state.preflight?.ok, false);

        await assert.rejects(() => controller.execute());
        assert.equal(spawn.calls.length, 1, "only the preflight spawn happened");
    });

    it("execute() throws when the preview succeeded but confirmation text doesn't match the group name", async () => {
        const spawn = new RecordingSpawn([ok(okPreflightJson), ok(expandPreviewJson)]);
        const controller = new ExpandController(spawn.fn, expandInput());

        await controller.runPreflight();
        const afterPreview = await controller.runPreview();
        assert.equal(afterPreview.step, "confirm");

        assert.equal(controller.canExecute(), false);
        await assert.rejects(() => controller.execute());

        controller.setConfirmationText("not-shr1");
        assert.equal(controller.canExecute(), false);
        await assert.rejects(() => controller.execute());

        assert.equal(spawn.calls.length, 2, "preflight + dry-run only -- real expand never spawned");
    });

    it("happy path: preflight -> dry-run preview -> matching confirmation -> real expand, in order", async () => {
        const spawn = new RecordingSpawn([ok(okPreflightJson), ok(expandPreviewJson), ok(expandedJson)]);
        const controller = new ExpandController(spawn.fn, expandInput());

        await controller.runPreflight();
        await controller.runPreview();
        controller.setConfirmationText("shr1");
        assert.equal(controller.canExecute(), true);

        const final = await controller.execute();
        assert.equal(final.step, "done");
        assert.equal(final.result?.layout_version, 2);
        assert.equal(final.result?.disk_count, 4);

        assert.equal(spawn.calls.length, 3);
        assert.ok(spawn.calls[0].argv.includes("preflight"));
        assert.ok(spawn.calls[1].argv.includes("--dry-run"));
        assert.ok(spawn.calls[2].argv.includes("expand"));
        assert.ok(spawn.calls[2].argv.includes("--yes"));
        assert.ok(!spawn.calls[2].argv.includes("--dry-run"));
    });

    it("a rejected real expand spawn surfaces the engine's stderr verbatim and lands on error, not done", async () => {
        const spawn = new RecordingSpawn([
            ok(okPreflightJson),
            ok(expandPreviewJson),
            fail("band 0 (md0) has background activity in progress (sync_action=check)"),
        ]);
        const controller = new ExpandController(spawn.fn, expandInput());
        await controller.runPreflight();
        await controller.runPreview();
        controller.setConfirmationText("shr1");

        const state = await controller.execute();
        assert.equal(state.step, "error");
        assert.equal(state.errorMessage, "band 0 (md0) has background activity in progress (sync_action=check)");
        assert.equal(state.result, null);
    });
});

const group = (overrides: Partial<GroupStatus> = {}): GroupStatus => ({
    name: "shr1",
    mode: "shr",
    layout_version: 1,
    mount_point: "/mnt/shr_data",
    fs_uuid: "abc-123",
    usable_bytes: 8_000_000_000_000,
    resize_pending: false,
    disks: ["ata-DISK1", "ata-DISK2", "ata-DISK3"],
    bands: [{
        index: 0,
        level: "raid5",
        md_name: "md0",
        usable_bytes: 8_000_000_000_000,
        resize_pending: false,
        members: ["sdb", "sdc", "sdd"],
        sync: null,
        last_scrub: null,
        scrub_in_progress: false,
    }],
    ...overrides,
});

describe("groupMemberDisks correlates group membership via md_name, not a shared id field", () => {
    it("returns only disks whose arrays include one of the group's band md_names", () => {
        const disks = [
            disk({ name: "sdb", arrays: ["md0"] }),
            disk({ name: "sdc", arrays: ["md0"] }),
            disk({ name: "sdd", arrays: ["md1"] }),
            disk({ name: "sde", arrays: [] }),
        ];
        assert.deepEqual(groupMemberDisks(group(), disks).map(d => d.name), ["sdb", "sdc"]);
    });

    it("returns an empty list when no disk backs any of the group's bands", () => {
        const disks = [disk({ name: "sdb", arrays: ["md9"] })];
        assert.deepEqual(groupMemberDisks(group(), disks), []);
    });
});

describe("disk replace: same-or-larger filtering and argument validation", () => {
    it("isValidReplacement rejects a smaller candidate, a disk already in an array, and the disk itself", () => {
        const oldDisk = disk({ name: "sdb", size: 4_000_000_000_000 });
        assert.equal(isValidReplacement(oldDisk, disk({ name: "sdc", size: 2_000_000_000_000 })), false);
        assert.equal(isValidReplacement(oldDisk, disk({ name: "sdd", size: 4_000_000_000_000, arrays: ["md0"] })), false);
        assert.equal(isValidReplacement(oldDisk, disk({ name: "sdb", size: 8_000_000_000_000 })), false);
        assert.equal(isValidReplacement(oldDisk, disk({ name: "sdc", size: 8_000_000_000_000 })), true);
        assert.equal(isValidReplacement(oldDisk, disk({ name: "sdc", size: 4_000_000_000_000 })), true);
    });

    it("isValidReplacement rejects when either size is unknown", () => {
        const oldDisk = disk({ name: "sdb", size: null });
        assert.equal(isValidReplacement(oldDisk, disk({ name: "sdc", size: 4_000_000_000_000 })), false);
        assert.equal(isValidReplacement(disk({ size: 4_000_000_000_000 }), disk({ name: "sdc", size: null })), false);
    });

    it("filterReplacementCandidates only returns valid candidates", () => {
        const oldDisk = disk({ name: "sdb", size: 4_000_000_000_000 });
        const pool = [
            disk({ name: "sdc", size: 2_000_000_000_000 }),
            disk({ name: "sdd", size: 6_000_000_000_000 }),
            disk({ name: "sde", size: 6_000_000_000_000, arrays: ["md0"] }),
        ];
        assert.deepEqual(filterReplacementCandidates(oldDisk, pool).map(d => d.name), ["sdd"]);
    });

    it("replaceArgs blocks a smaller replacement disk before spawn", () => {
        assert.throws(
            () => replaceArgs(replaceInput({ oldSize: 4_000_000_000_000, newSize: 2_000_000_000_000 })),
            /must be the same size as the old one or larger/,
        );
    });

    it("replaceArgs blocks unknown sizes, an empty group name, and old === new", () => {
        assert.throws(() => replaceArgs(replaceInput({ oldSize: null })));
        assert.throws(() => replaceArgs(replaceInput({ newSize: null })));
        assert.throws(() => replaceArgs(replaceInput({ groupName: "" })));
        assert.throws(() => replaceArgs(replaceInput({ oldId: "same", newId: "same" })));
    });

    it("a valid, equal-size replacement is allowed (same-or-larger, not strictly-larger)", () => {
        const call = replaceArgs(replaceInput({ oldSize: 4_000_000_000_000, newSize: 4_000_000_000_000 }));
        assert.deepEqual(call.argv, ["shr-rs", "disk", "replace", "--name", "shr1", "--old", "ata-DISK1", "--new", "ata-DISK9", "--yes"]);
    });
});

describe("hasStableId", () => {
    it("is true only for a non-empty string id", () => {
        assert.equal(hasStableId(disk({ id: "ata-LOOP_DISK_10" })), true);
        assert.equal(hasStableId(disk({ id: null })), false);
        assert.equal(hasStableId(disk({})), false, "absent id (the disk() factory's own default) must not be treated as stable");
        assert.equal(hasStableId(disk({ id: "" })), false);
    });
});

// The actual bug -- Cockpit's disk replace sent `DiskStatus.name` (the
// kernel name, e.g. "loop10") as both `--old` and `--new`, but the engine's
// `replace_disk` matches `--old` literally against `StateDisk::id` (the
// by-id name, e.g. "ata-LOOP_DISK_10") and never resolves it -- confirmed
// against a live array: `disk replace --old loop10 ...` fails with `disk
// 'loop10' is not a member of group 'demo1'`, while `--old ata-LOOP_DISK_10
// ...` succeeds. So replace had never once worked from Cockpit.
//
// Every existing test above this point used `replaceInput()`, whose
// `oldId`/`newId` are hand-picked strings ("ata-DISK1"/"ata-DISK9") that
// never come from a `DiskStatus.name`/`.id` pair -- and `disk()`'s own
// factory never sets `.id` at all unless a test overrides it. Neither
// fixture could ever have caught this: `replaceArgs`'s tests exercise
// argument validation on an already-correct input, not the step that
// built that input from a *chosen disk pair* in the first place -- which
// used to live inline in `actionsDialogs.tsx`'s `ReplaceDialog.proceed()`,
// entirely untested. `buildReplaceInput` is that step, pulled out to
// actions.ts specifically so it can be pinned here.
describe("buildReplaceInput (the actual proceed()-building step, not just replaceArgs on an already-correct input)", () => {
    it("uses .id, never .name, when they differ -- proves the fix for the confirmed live-array failure", () => {
        const oldDisk = disk({ name: "loop10", id: "ata-LOOP_DISK_10", size: 4_000_000_000_000 });
        const newDisk = disk({ name: "loop13", id: "ata-LOOP_DISK_13", size: 4_000_000_000_000 });
        const input = buildReplaceInput("demo1", oldDisk, newDisk);
        assert.deepEqual(input, {
            groupName: "demo1",
            oldId: "ata-LOOP_DISK_10",
            newId: "ata-LOOP_DISK_13",
            oldSize: 4_000_000_000_000,
            newSize: 4_000_000_000_000,
        });
        // Chained into the real argv-builder, matching the exact CLI
        // invocation confirmed to succeed against the live array.
        const call = replaceArgs(input);
        assert.deepEqual(
            call.argv,
            ["shr-rs", "disk", "replace", "--name", "demo1", "--old", "ata-LOOP_DISK_10", "--new", "ata-LOOP_DISK_13", "--yes"],
        );
    });

    it("never falls back to .name when a disk has no stable id -- oldId/newId are empty, not the kernel name", () => {
        const oldDiskNoId = disk({ name: "loop10", id: null, size: 4_000_000_000_000 });
        const newDisk = disk({ name: "loop13", id: "ata-LOOP_DISK_13", size: 4_000_000_000_000 });
        const input = buildReplaceInput("demo1", oldDiskNoId, newDisk);
        assert.equal(input.oldId, "", "must not silently fall back to the kernel name when id is missing");
        assert.notEqual(input.oldId, "loop10");

        const newDiskNoId = disk({ name: "loop13", size: 4_000_000_000_000 }); // id absent (disk()'s own default)
        const input2 = buildReplaceInput("demo1", disk({ name: "loop10", id: "ata-LOOP_DISK_10" }), newDiskNoId);
        assert.equal(input2.newId, "");
        assert.notEqual(input2.newId, "loop13");
    });

    it("returns empty ids (never a name) for null old/new disks, so replaceArgs still throws its own clear message", () => {
        const input = buildReplaceInput("demo1", null, null);
        assert.equal(input.oldId, "");
        assert.equal(input.newId, "");
        assert.throws(() => replaceArgs(input), /Select the disk to replace/);
    });
});

describe("recompress argument validation", () => {
    it("accepts algo and algo:level forms", () => {
        assert.doesNotThrow(() => recompressArgs({ groupName: "shr1", compression: "zstd" }));
        assert.doesNotThrow(() => recompressArgs({ groupName: "shr1", compression: "zstd:3" }));
    });

    it("rejects malformed compression strings and an empty group name", () => {
        assert.throws(() => recompressArgs({ groupName: "shr1", compression: "" }));
        assert.throws(() => recompressArgs({ groupName: "shr1", compression: "zstd:" }));
        assert.throws(() => recompressArgs({ groupName: "shr1", compression: "zstd level 3" }));
        assert.throws(() => recompressArgs({ groupName: "shr1", compression: "; rm -rf /" }));
        assert.throws(() => recompressArgs({ groupName: "", compression: "zstd:3" }));
    });
});

describe("snapshot create argument validation", () => {
    it("rejects an empty snapshot name, a name containing /, and an empty group", () => {
        assert.throws(() => snapshotCreateArgs({ groupName: "shr1", snapshotName: "" }));
        assert.throws(() => snapshotCreateArgs({ groupName: "shr1", snapshotName: "a/b" }));
        assert.throws(() => snapshotCreateArgs({ groupName: "", snapshotName: "before-upgrade" }));
    });

    it("builds the expected argv on valid input", () => {
        const call = snapshotCreateArgs({ groupName: "shr1", snapshotName: "before-upgrade" });
        assert.deepEqual(call.argv, ["shr-rs", "fs", "snapshot", "create", "before-upgrade", "--group", "shr1"]);
    });
});

describe("scrub argument validation", () => {
    it("rejects an empty group name for start/status/cancel", () => {
        assert.throws(() => scrubStartArgs(""));
        assert.throws(() => scrubStatusArgs(""));
        assert.throws(() => scrubCancelArgs(""));
    });

    it("status carries --json, start/cancel do not (shr-cli prints plain text for those unconditionally)", () => {
        assert.ok(scrubStatusArgs("shr1").argv.includes("--json"));
        assert.ok(!scrubStartArgs("shr1").argv.includes("--json"));
        assert.ok(!scrubCancelArgs("shr1").argv.includes("--json"));
    });
});

describe("parseScrubStatus trusts the backend verbatim", () => {
    it("parses a finished, clean scrub", () => {
        const report = parseScrubStatus(JSON.stringify({ group: "shr1", running: false, error_count: 0 }));
        assert.deepEqual(report, { group_name: "shr1", running: false, error_count: 0 });
    });

    it("surfaces a nonzero error_count rather than hiding it", () => {
        const report = parseScrubStatus(JSON.stringify({ group: "shr1", running: false, error_count: 3 }));
        assert.equal(report.error_count, 3);
    });

    it("throws on malformed JSON and on an error payload, rather than defaulting", () => {
        assert.throws(() => parseScrubStatus("not json"));
        assert.throws(() => parseScrubStatus(JSON.stringify({ error: "no scrub has ever run for group `shr1`" })), /no scrub has ever run/);
    });
});

// `shr-rs reconcile` finishes an LVM/Btrfs resize a previous `expand`
// had to defer while its mdadm reshape was running -- the documented,
// verified-on-real-hardware remedy for the `resize_pending` warning badge
// both frontends already render, but neither frontend could reach it before
// this. Unlike every other action here, reconcile is not scoped to one
// group (shr-cli's `Command::Reconcile` variant takes no `--name`) -- it
// walks every group's pending bands in one call.
describe("reconcile argument validation", () => {
    it("takes no input and builds the bare argv every time", () => {
        assert.deepEqual(reconcileArgs().argv, ["shr-rs", "reconcile"]);
        assert.deepEqual(reconcileArgs().argv, reconcileArgs().argv, "no hidden per-call variance");
    });
});

// Closes the one CLI operation with no Cockpit path at all: `shr-rs destroy`
// unmounts, removes LV/VG/PVs, stops every mdadm array, drops the group from
// state.toml, and regenerates mdadm.conf/fstab -- shr-cli's own --help warns
// a hand-teardown "leaves orphaned managed-block entries behind". Gets the
// same TypedConfirmController shape as disk replace/recompress (the
// lesson applies here too: a test must exercise `proceed()`/`execute()`
// end-to-end with the id/name the dialog was actually opened for, not just
// call the argv builder with hand-picked strings).
describe("destroy argument validation", () => {
    it("rejects an empty group name", () => {
        assert.throws(() => destroyArgs({ groupName: "", zeroSuperblocks: false }));
        assert.throws(() => destroyArgs({ groupName: "   ", zeroSuperblocks: false }));
    });

    it("always carries --name, --yes, and --json", () => {
        const call = destroyArgs({ groupName: "shr1", zeroSuperblocks: false });
        assert.ok(call.argv.includes("destroy"));
        assert.deepEqual(call.argv.slice(call.argv.indexOf("--name"), call.argv.indexOf("--name") + 2), ["--name", "shr1"]);
        assert.ok(call.argv.includes("--yes"), "the UI's own typed-confirmation gate already confirmed -- --yes must always be present");
        assert.ok(call.argv.includes("--json"));
    });

    // Both states asserted, not just the checked one -- a test that only
    // checks `zeroSuperblocks: true` would not prove the flag is actually
    // conditional (the lesson: assert what the test claims to assert).
    //
    // Neither position may be expressed by OMITTING a flag: `--yes` makes
    // this non-interactive, and `destroy` refuses to choose the superblock
    // decision for a caller that never states one. Leaving the unchecked
    // case bare would make every Cockpit destroy fail outright.
    it("spells out the superblock decision in both checkbox positions", () => {
        const off = destroyArgs({ groupName: "shr1", zeroSuperblocks: false });
        assert.ok(!off.argv.includes("--zero-superblocks"));
        assert.ok(off.argv.includes("--no-zero-superblocks"), "unchecked must be stated, not omitted");

        const on = destroyArgs({ groupName: "shr1", zeroSuperblocks: true });
        assert.ok(on.argv.includes("--zero-superblocks"));
        assert.ok(!on.argv.includes("--no-zero-superblocks"));
    });

    it("builds the exact expected argv on valid input, both zero-superblocks states", () => {
        assert.deepEqual(
            destroyArgs({ groupName: "shr1", zeroSuperblocks: false }).argv,
            ["shr-rs", "destroy", "--name", "shr1", "--yes", "--json", "--no-zero-superblocks"],
        );
        assert.deepEqual(
            destroyArgs({ groupName: "shr1", zeroSuperblocks: true }).argv,
            ["shr-rs", "destroy", "--name", "shr1", "--yes", "--json", "--zero-superblocks"],
        );
    });
});

describe("parseDestroyResult trusts the backend verbatim", () => {
    it("parses the {\"destroyed\": \"<group>\"} shape shr-cli's json branch emits", () => {
        const result = parseDestroyResult(JSON.stringify({ destroyed: "shr1" }));
        assert.deepEqual(result, { destroyed: "shr1" });
    });

    it("throws on malformed JSON and on a missing/non-string destroyed field, rather than defaulting", () => {
        assert.throws(() => parseDestroyResult("not json"));
        assert.throws(() => parseDestroyResult(JSON.stringify({})));
        assert.throws(() => parseDestroyResult(JSON.stringify({ destroyed: 123 })));
    });
});

describe("TypedConfirmController (destroy shape): proceed()/execute() end-to-end, not just the argv builder", () => {
    const destroyInput = (overrides: Partial<DestroyInput> = {}): DestroyInput => ({
        groupName: "shr1",
        zeroSuperblocks: false,
        ...overrides,
    });

    it("execute() throws before confirmation text is typed, and again when it doesn't match -- nothing spawns either time", async () => {
        const spawn = new RecordingSpawn([]);
        const controller = new TypedConfirmController(spawn.fn, destroyInput(), destroyArgs, i => i.groupName, parseDestroyResult);

        controller.proceedToConfirm();
        assert.equal(controller.canExecute(), false);
        await assert.rejects(() => controller.execute());

        controller.setConfirmationText("wrong-name");
        assert.equal(controller.canExecute(), false);
        await assert.rejects(() => controller.execute());

        assert.equal(spawn.calls.length, 0, "no destroy spawn without a matching typed confirmation");
    });

    it("a matching confirmation lets execute() proceed, spawning the exact argv opened for this group -- --name matches the group the dialog was opened for", async () => {
        const spawn = new RecordingSpawn([ok(JSON.stringify({ destroyed: "shr1" }))]);
        const input = destroyInput({ groupName: "shr1" });
        const controller = new TypedConfirmController(spawn.fn, input, destroyArgs, i => i.groupName, parseDestroyResult);

        controller.proceedToConfirm();
        controller.setConfirmationText("shr1");
        assert.equal(controller.canExecute(), true);

        const state = await controller.execute();
        assert.equal(state.step, "done");
        assert.deepEqual(state.result, { destroyed: "shr1" });
        assert.equal(spawn.calls.length, 1);
        assert.deepEqual(
            spawn.calls[0].argv,
            ["shr-rs", "destroy", "--name", "shr1", "--yes", "--json", "--no-zero-superblocks"],
        );
        assert.ok(spawn.calls[0].argv.includes("shr1"), "must spawn --name for the group the dialog was actually opened for");
    });

    it("--zero-superblocks travels from input through to the real spawn when checked", async () => {
        const spawn = new RecordingSpawn([ok(JSON.stringify({ destroyed: "shr1" }))]);
        const controller = new TypedConfirmController(
            spawn.fn, destroyInput({ zeroSuperblocks: true }), destroyArgs, i => i.groupName, parseDestroyResult,
        );
        controller.proceedToConfirm();
        controller.setConfirmationText("shr1");
        await controller.execute();
        assert.ok(spawn.calls[0].argv.includes("--zero-superblocks"));
    });

    it("a rejected destroy spawn surfaces the backend's own message verbatim and lands on error, not done", async () => {
        const spawn = new RecordingSpawn([fail("group `shr1` has a mounted filesystem at `/mnt/shr_data` -- unmount failed: target is busy")]);
        const controller = new TypedConfirmController(spawn.fn, destroyInput(), destroyArgs, i => i.groupName, parseDestroyResult);
        controller.proceedToConfirm();
        controller.setConfirmationText("shr1");
        const state = await controller.execute();
        assert.equal(state.step, "error");
        assert.equal(state.errorMessage, "group `shr1` has a mounted filesystem at `/mnt/shr_data` -- unmount failed: target is busy");
        assert.equal(state.result, null);
    });
});

describe("parseTextResult", () => {
    it("trims stdout and does not otherwise interpret it", () => {
        assert.equal(parseTextResult("scrub started\n"), "scrub started");
        assert.equal(parseTextResult("  installed and enabled 3 timer unit(s)  \n"), "installed and enabled 3 timer unit(s)");
    });
});

describe("isConfirmationValid", () => {
    it("requires an exact, non-empty, case-sensitive match", () => {
        assert.equal(isConfirmationValid("", "shr1"), false);
        assert.equal(isConfirmationValid("SHR1", "shr1"), false);
        assert.equal(isConfirmationValid("shr1 ", "shr1"), false);
        assert.equal(isConfirmationValid("shr1", "shr1"), true);
    });
});

describe("TypedConfirmController (disk replace / recompress shape): no execute before a matching typed confirmation", () => {
    it("proceedToConfirm() throws on invalid input and never reaches confirm/spawn", () => {
        const spawn = new RecordingSpawn([]);
        const controller = new TypedConfirmController(
            spawn.fn,
            replaceInput({ oldSize: 4_000_000_000_000, newSize: 1_000_000_000_000 }),
            replaceArgs,
            i => i.groupName,
            parseTextResult,
        );
        assert.throws(() => controller.proceedToConfirm());
        assert.equal(controller.state.step, "review");
        assert.equal(spawn.calls.length, 0);
    });

    it("execute() throws before confirmation text is typed, and again when it doesn't match", async () => {
        const spawn = new RecordingSpawn([]);
        const controller = new TypedConfirmController(spawn.fn, replaceInput(), replaceArgs, i => i.groupName, parseTextResult);

        controller.proceedToConfirm();
        assert.equal(controller.canExecute(), false);
        await assert.rejects(() => controller.execute());

        controller.setConfirmationText("wrong-name");
        assert.equal(controller.canExecute(), false);
        await assert.rejects(() => controller.execute());

        assert.equal(spawn.calls.length, 0, "no replace spawn without a matching typed confirmation");
    });

    it("happy path spawns exactly once, with the exact argv replaceArgs would build, after a matching confirmation", async () => {
        const spawn = new RecordingSpawn([ok("disk `ata-DISK1` replaced with `ata-DISK9` in group `shr1`")]);
        const input = replaceInput();
        const controller = new TypedConfirmController(spawn.fn, input, replaceArgs, i => i.groupName, parseTextResult);

        controller.proceedToConfirm();
        controller.setConfirmationText("shr1");
        assert.equal(controller.canExecute(), true);

        const state = await controller.execute();
        assert.equal(state.step, "done");
        assert.equal(state.result, "disk `ata-DISK1` replaced with `ata-DISK9` in group `shr1`");
        assert.equal(spawn.calls.length, 1);
        assert.deepEqual(spawn.calls[0].argv, replaceArgs(input).argv);
    });

    it("a rejected recompress spawn surfaces stderr verbatim and lands on error, not done", async () => {
        const spawn = new RecordingSpawn([fail("btrfs property set: No such file or directory")]);
        const input = { groupName: "shr1", compression: "zstd:5" };
        const controller = new TypedConfirmController(spawn.fn, input, recompressArgs, i => i.groupName, parseTextResult);

        controller.proceedToConfirm();
        controller.setConfirmationText("shr1");
        const state = await controller.execute();
        assert.equal(state.step, "error");
        assert.equal(state.errorMessage, "btrfs property set: No such file or directory");
        assert.equal(state.result, null);
    });
});

describe("SimpleActionController (scrub start/cancel, snapshot, schedule install shape)", () => {
    it("proceedToConfirm() throws on invalid input (empty group name) and never spawns", () => {
        const spawn = new RecordingSpawn([]);
        const controller = new SimpleActionController(spawn.fn, "", scrubStartArgs, parseTextResult);
        assert.throws(() => controller.proceedToConfirm());
        assert.equal(spawn.calls.length, 0);
    });

    it("execute() throws before confirm() is called, and no spawn happens", async () => {
        const spawn = new RecordingSpawn([]);
        const controller = new SimpleActionController(spawn.fn, "shr1", scrubStartArgs, parseTextResult);
        controller.proceedToConfirm();
        assert.equal(controller.canExecute(), false);
        await assert.rejects(() => controller.execute());
        assert.equal(spawn.calls.length, 0);
    });

    it("happy path: proceedToConfirm -> confirm -> execute spawns exactly once", async () => {
        const spawn = new RecordingSpawn([ok("scrub started\n")]);
        const controller = new SimpleActionController(spawn.fn, "shr1", scrubStartArgs, parseTextResult);
        controller.proceedToConfirm();
        controller.confirm();
        assert.equal(controller.canExecute(), true);
        const state = await controller.execute();
        assert.equal(state.step, "done");
        assert.equal(state.result, "scrub started");
        assert.equal(spawn.calls.length, 1);
        assert.deepEqual(spawn.calls[0].argv, scrubStartArgs("shr1").argv);
    });

    it("a rejected scrub cancel spawn surfaces stderr verbatim and lands on error", async () => {
        const spawn = new RecordingSpawn([fail("no scrub is currently running for group `shr1`")]);
        const controller = new SimpleActionController(spawn.fn, "shr1", scrubCancelArgs, parseTextResult);
        controller.proceedToConfirm();
        controller.confirm();
        const state = await controller.execute();
        assert.equal(state.step, "error");
        assert.equal(state.errorMessage, "no scrub is currently running for group `shr1`");
    });

    it("schedule install takes no group name and spawns the expected argv once confirmed", async () => {
        const spawn = new RecordingSpawn([ok("installed and enabled 3 timer unit(s)\n")]);
        const controller = new SimpleActionController(spawn.fn, undefined, scheduleInstallArgs, parseTextResult);
        controller.proceedToConfirm();
        controller.confirm();
        const state = await controller.execute();
        assert.equal(state.step, "done");
        assert.equal(state.result, "installed and enabled 3 timer unit(s)");
        assert.deepEqual(spawn.calls[0].argv, ["shr-rs", "schedule", "install"]);
    });

    // Reconcile is idempotent and non-destructive (it only finishes
    // bookkeeping for a reshape a prior `expand` already committed --
    // `OrchestrationEngine::reconcile`'s own doc comment is explicit that it
    // never starts a new destructive action), so it gets the same shape as
    // schedule install: a single explicit confirm, no typed group-name gate.
    it("reconcile takes no group name and spawns the expected argv once confirmed", async () => {
        const spawn = new RecordingSpawn([ok("Reconcile: group `shr1` band 0 (md0): completed the deferred resize\n")]);
        const controller = new SimpleActionController(spawn.fn, undefined, reconcileArgs, parseTextResult);
        controller.proceedToConfirm();
        controller.confirm();
        const state = await controller.execute();
        assert.equal(state.step, "done");
        assert.equal(state.result, "Reconcile: group `shr1` band 0 (md0): completed the deferred resize");
        assert.deepEqual(spawn.calls[0].argv, ["shr-rs", "reconcile"]);
    });

    it("a rejected reconcile spawn surfaces stderr verbatim and lands on error, not done", async () => {
        const spawn = new RecordingSpawn([fail("mdadm --detail failed: /dev/md0: No such file or directory")]);
        const controller = new SimpleActionController(spawn.fn, undefined, reconcileArgs, parseTextResult);
        controller.proceedToConfirm();
        controller.confirm();
        const state = await controller.execute();
        assert.equal(state.step, "error");
        assert.equal(state.errorMessage, "mdadm --detail failed: /dev/md0: No such file or directory");
        assert.equal(state.result, null);
    });
});
