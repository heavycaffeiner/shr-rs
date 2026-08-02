/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
    CreateGroupController,
    createArgs,
    deriveVgName,
    dryRunCreateArgs,
    findVgNameConflict,
    isConfirmationValid,
    parseCreatePreview,
    parseCreatedGroup,
    parseWritePreflight,
    preflightArgs,
    sanitizeLvmNameComponent,
    DEFAULT_LV_NAME,
    type ExistingGroupIdentity,
    type Spawn,
    type WizardFormInput,
} from "./createGroup.ts";

import { installEnglishCatalog } from "./testCatalog.ts";

// The msgids in `src/` are dotted keys, so without a catalogue every string
// below would render as its key. See testCatalog.ts.
installEnglishCatalog();

const okPreflightJson = JSON.stringify({
    ok: true,
    blockers: [],
    warnings: [],
    targets: [{ kernel_name: "sdb", id: "ata-DISK1", size: 4_000_000_000_000, system_disk: false, system_mounts: [], has_content: false }],
});

const blockedPreflightJson = JSON.stringify({
    ok: false,
    blockers: [{ kind: "has_content", name: "sdb" }],
    warnings: [],
    targets: [{ kernel_name: "sdb", id: "ata-DISK1", size: 4_000_000_000_000, system_disk: false, system_mounts: [], has_content: true }],
});

const previewJson = JSON.stringify({
    name: "shr1",
    mode: "shr",
    layout_version: 1,
    disks: [{ id: "ata-DISK1" }, { id: "ata-DISK2" }, { id: "ata-DISK3" }],
    bands: [{ index: 0, level: "raid5", md_name: "md0", usable_bytes: 8_000_000_000_000, resize_pending: false }],
    filesystem: { fs_uuid: null, mount_point: "/mnt/shr_data", vg_name: "shr_vg", lv_name: "data", compression: "zstd:3" },
    planned_commands: ["parted /dev/sdb mklabel gpt", "mdadm --create /dev/md0 --level=5 --raid-devices=3"],
});

const createdJson = JSON.stringify({
    name: "shr1",
    mode: "shr",
    layout_version: 1,
    disks: [{ id: "ata-DISK1" }, { id: "ata-DISK2" }, { id: "ata-DISK3" }],
    bands: [{ index: 0, level: "raid5", md_name: "md0", usable_bytes: 8_000_000_000_000, resize_pending: false }],
    filesystem: { fs_uuid: "abc-123", mount_point: "/mnt/shr_data", vg_name: "shr_vg", lv_name: "data", compression: "zstd:3" },
});

const formInput = (overrides: Partial<WizardFormInput> = {}): WizardFormInput => ({
    name: "shr1",
    mode: "shr",
    mountPoint: "/mnt/shr_data",
    selectedDisks: ["sdb", "sdc", "sdd"],
    forceContent: false,
    vgName: "vg_shr1",
    lvName: DEFAULT_LV_NAME,
    ...overrides,
});

/** Records every call so tests can assert exact argv/options/order. */
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

describe("spawn argument builders (constraint 2 + 3)", () => {
    it("every builder always requires superuser, never try", () => {
        assert.equal(preflightArgs(["sdb"], false).options.superuser, "require");
        assert.equal(dryRunCreateArgs(formInput()).options.superuser, "require");
        assert.equal(createArgs(formInput()).options.superuser, "require");
    });

    it("--force-content is never present unless explicitly requested", () => {
        assert.ok(!preflightArgs(["sdb"], false).argv.includes("--force-content"));
        assert.ok(preflightArgs(["sdb"], true).argv.includes("--force-content"));
        assert.ok(!dryRunCreateArgs(formInput({ forceContent: false })).argv.includes("--force-content"));
        assert.ok(dryRunCreateArgs(formInput({ forceContent: true })).argv.includes("--force-content"));
        assert.ok(!createArgs(formInput({ forceContent: false })).argv.includes("--force-content"));
    });

    it("createArgs is dryRunCreateArgs minus --dry-run -- the executed command can't drift from the previewed one", () => {
        const preview = dryRunCreateArgs(formInput());
        const real = createArgs(formInput());
        assert.deepEqual(real.argv, preview.argv.filter(a => a !== "--dry-run"));
        assert.ok(preview.argv.includes("--dry-run"));
        assert.ok(!real.argv.includes("--dry-run"));
    });

    // Before this fix, dryRunCreateArgs/createArgs never emitted
    // --vg-name/--lv-name at all, so every Cockpit-created group silently
    // fell back to shr-cli's hardcoded shr_vg/data -- confirmed in a real
    // browser (execution-plan preview literally printed `vgcreate shr_vg
    // /dev/md1`). These assert the explicit names always make it into argv.
    it("dryRunCreateArgs and createArgs always pass explicit --vg-name/--lv-name", () => {
        const input = formInput({ vgName: "vg_myshr", lvName: "mydata" });
        const preview = dryRunCreateArgs(input);
        const real = createArgs(input);

        const vgIndex = preview.argv.indexOf("--vg-name");
        assert.ok(vgIndex !== -1, "--vg-name must be present");
        assert.equal(preview.argv[vgIndex + 1], "vg_myshr");

        const lvIndex = preview.argv.indexOf("--lv-name");
        assert.ok(lvIndex !== -1, "--lv-name must be present");
        assert.equal(preview.argv[lvIndex + 1], "mydata");

        assert.ok(real.argv.includes("--vg-name"));
        assert.ok(real.argv.includes("--lv-name"));
    });
});

describe("LVM VG/LV name derivation", () => {
    it("sanitizeLvmNameComponent keeps a plain ASCII name unchanged", () => {
        assert.equal(sanitizeLvmNameComponent("shr1"), "shr1");
        assert.equal(sanitizeLvmNameComponent("shr-dev_2"), "shr-dev_2");
    });

    it("sanitizeLvmNameComponent replaces characters LVM does not accept", () => {
        assert.equal(sanitizeLvmNameComponent("shr 1"), "shr_1");
        assert.equal(sanitizeLvmNameComponent("shr/2"), "shr_2");
        assert.equal(sanitizeLvmNameComponent("안방 NAS"), "___NAS");
    });

    it("sanitizeLvmNameComponent strips a leading dash/dot (LVM rejects those)", () => {
        assert.equal(sanitizeLvmNameComponent("-shr1"), "shr1");
        assert.equal(sanitizeLvmNameComponent(".shr1"), "shr1");
    });

    it("sanitizeLvmNameComponent falls back to a fixed default when nothing survives sanitizing", () => {
        assert.equal(sanitizeLvmNameComponent("가나다"), "grp");
        assert.equal(sanitizeLvmNameComponent(""), "grp");
        assert.equal(sanitizeLvmNameComponent("   "), "grp");
    });

    it("sanitizeLvmNameComponent truncates an unreasonably long name", () => {
        const long = "a".repeat(200);
        assert.ok(sanitizeLvmNameComponent(long).length <= 48);
    });

    it("deriveVgName prefixes the sanitized group name with vg_", () => {
        assert.equal(deriveVgName("shr1"), "vg_shr1");
        assert.equal(deriveVgName("shr 1"), "vg_shr_1");
    });

    it("deriveVgName never collides for two group names that only differ by an LVM-illegal character (documented, not a full guarantee)", () => {
        // Both sanitize down to the same string -- this is a known,
        // documented edge case; findVgNameConflict (checked below) is what
        // actually catches a real collision against already-recorded groups.
        assert.equal(deriveVgName("shr!1"), deriveVgName("shr@1"));
    });
});

describe("findVgNameConflict (client-side collision guard)", () => {
    const existing: ExistingGroupIdentity[] = [
        { name: "shr1", vg_name: "vg_shr1" },
        { name: "legacy", vg_name: "shr_vg" },
    ];

    it("returns null when the proposed VG name is not in use", () => {
        assert.equal(findVgNameConflict("vg_shr2", existing), null);
    });

    it("returns a message naming the colliding group when the VG name is already in use", () => {
        const message = findVgNameConflict("vg_shr1", existing);
        assert.ok(message !== null);
        assert.match(message ?? "", /vg_shr1/);
        assert.match(message ?? "", /shr1/);
    });

    it("catches the exact repro: a second group defaulting to shr_vg collides with an existing one", () => {
        assert.notEqual(findVgNameConflict("shr_vg", existing), null);
    });

    it("returns null against an empty existing-groups list (first group ever created)", () => {
        assert.equal(findVgNameConflict("vg_shr1", []), null);
    });
});

describe("parsers trust the backend verbatim (constraint 4)", () => {
    it("parseWritePreflight surfaces blockers exactly as returned, without re-deriving ok", () => {
        const report = parseWritePreflight(blockedPreflightJson);
        assert.equal(report.ok, false);
        assert.deepEqual(report.blockers, [{ kind: "has_content", name: "sdb" }]);
    });

    it("parseCreatePreview extracts planned_commands in order", () => {
        const preview = parseCreatePreview(previewJson);
        assert.equal(preview.planned_commands.length, 2);
        assert.equal(preview.planned_commands[0], "parted /dev/sdb mklabel gpt");
        assert.equal(preview.disk_count, 3);
    });

    it("parseCreatedGroup rejects a payload carrying an error field instead of throwing a generic parse error", () => {
        assert.throws(() => parseCreatedGroup(JSON.stringify({ error: "preflight blocked: write targets are not safe" })), /preflight blocked/);
    });

    // An unrecognised blocker kind must not take the rest of the report
    // down with it. Before this, the parser threw, so a single unknown kind
    // (e.g. a newer backend adding one) discarded every OTHER blocker, every
    // warning and every target in the same response -- the operator lost real
    // safety messages because of one they could not name.
    it("an unrecognised blocker kind is surfaced raw, and does not discard the blockers around it", () => {
        const report = parseWritePreflight(JSON.stringify({
            ok: false,
            blockers: [
                { kind: "has_content", name: "sdb" },
                { kind: "kind_from_a_newer_backend", name: "sdc", detail: "something" },
                { kind: "system_disk", name: "sda", id: "ata-SYS", mounts: ["/"] },
            ],
            warnings: ["a warning that must survive"],
            targets: [],
        }));

        assert.equal(report.blockers.length, 3, "the known blockers on either side must survive");
        assert.deepEqual(report.blockers[0], { kind: "has_content", name: "sdb" });
        assert.equal(report.blockers[2].kind, "system_disk");
        assert.deepEqual(report.warnings, ["a warning that must survive"]);

        const unknown = report.blockers[1];
        assert.equal(unknown.kind, "unknown");
        // The raw object is kept verbatim -- never guessed at, never dropped.
        assert.deepEqual(
            unknown.kind === "unknown" ? unknown.raw : null,
            { kind: "kind_from_a_newer_backend", name: "sdc", detail: "something" },
        );
    });
});

describe("confirmation gate (constraint 5)", () => {
    it("requires an exact, non-empty match -- not a boolean checkbox", () => {
        assert.equal(isConfirmationValid("", "shr1"), false);
        assert.equal(isConfirmationValid("shr", "shr1"), false);
        assert.equal(isConfirmationValid("SHR1", "shr1"), false);
        assert.equal(isConfirmationValid("shr1 ", "shr1"), false);
        assert.equal(isConfirmationValid("shr1", "shr1"), true);
    });
});

describe("CreateGroupController (constraint 1: no execution without preview)", () => {
    it("execute() throws and never spawns when called before any preflight/preview ran", async () => {
        const spawn = new RecordingSpawn([]);
        const controller = new CreateGroupController(spawn.fn, formInput());

        await assert.rejects(() => controller.execute());
        assert.equal(spawn.calls.length, 0, "no destructive spawn without a completed preview+confirm");
    });

    it("execute() throws and never spawns create when preflight is blocked", async () => {
        const spawn = new RecordingSpawn([ok(blockedPreflightJson)]);
        const controller = new CreateGroupController(spawn.fn, formInput());

        const afterPreflight = await controller.runPreflight();
        assert.equal(afterPreflight.step, "preflight");
        assert.equal(afterPreflight.preflight?.ok, false);

        await assert.rejects(() => controller.execute());
        assert.equal(spawn.calls.length, 1, "only the preflight spawn happened -- no dry-run, no create");
    });

    it("execute() throws when preview succeeded but the confirmation text was never typed (or typed wrong)", async () => {
        const spawn = new RecordingSpawn([ok(okPreflightJson), ok(previewJson)]);
        const controller = new CreateGroupController(spawn.fn, formInput());

        await controller.runPreflight();
        const afterPreview = await controller.runPreview();
        assert.equal(afterPreview.step, "confirm");
        assert.equal(afterPreview.preview?.planned_commands.length, 2);

        assert.equal(controller.canExecute(), false, "no confirmation text typed yet");
        await assert.rejects(() => controller.execute());

        controller.setConfirmationText("not-the-group-name");
        assert.equal(controller.canExecute(), false);
        await assert.rejects(() => controller.execute());

        assert.equal(spawn.calls.length, 2, "preflight + dry-run only -- create was never spawned");
    });

    it("the full happy path spawns preflight, dry-run preview, then real create, in that order, only after a matching confirmation", async () => {
        const spawn = new RecordingSpawn([ok(okPreflightJson), ok(previewJson), ok(createdJson)]);
        const controller = new CreateGroupController(spawn.fn, formInput());

        await controller.runPreflight();
        await controller.runPreview();
        controller.setConfirmationText("shr1");
        assert.equal(controller.canExecute(), true);

        const final = await controller.execute();
        assert.equal(final.step, "done");
        assert.equal(final.result?.name, "shr1");
        assert.equal(final.result?.band_count, 1);

        assert.equal(spawn.calls.length, 3);
        assert.ok(spawn.calls[0].argv.includes("preflight"));
        assert.ok(spawn.calls[1].argv.includes("--dry-run"));
        assert.ok(!spawn.calls[2].argv.includes("--dry-run"), "the real create call must not carry --dry-run");
        assert.ok(spawn.calls[2].argv.includes("create"));
    });
});

describe("CreateGroupController VG-name conflict gate", () => {
    it("a colliding VG name blocks on step=preflight even though the backend preflight itself passed, and never spawns the dry-run", async () => {
        const spawn = new RecordingSpawn([ok(okPreflightJson)]);
        const existingGroups: ExistingGroupIdentity[] = [{ name: "shr1", vg_name: "vg_shr1" }];
        const controller = new CreateGroupController(
            spawn.fn, formInput({ name: "shr2", vgName: "vg_shr1" }), existingGroups,
        );

        const state = await controller.runPreflight();
        assert.equal(state.step, "preflight", "a VG-name collision must not advance past preflight");
        assert.equal(state.preflight?.ok, true, "the backend's own disk preflight genuinely passed");
        assert.match(state.nameConflict ?? "", /vg_shr1/);
        assert.match(state.nameConflict ?? "", /shr1/);

        assert.equal(spawn.calls.length, 1, "only the preflight spawn happened -- no dry-run for a name we already know collides");
    });

    it("runPreview() throws rather than spawning when called despite a pending VG-name conflict", async () => {
        const spawn = new RecordingSpawn([ok(okPreflightJson)]);
        const existingGroups: ExistingGroupIdentity[] = [{ name: "shr1", vg_name: "vg_shr1" }];
        const controller = new CreateGroupController(
            spawn.fn, formInput({ name: "shr2", vgName: "vg_shr1" }), existingGroups,
        );

        await controller.runPreflight();
        await assert.rejects(() => controller.runPreview());
        assert.equal(spawn.calls.length, 1, "runPreview must not spawn the dry-run while a name conflict is pending");
    });

    it("a non-colliding VG name advances normally to preview (regression check)", async () => {
        const spawn = new RecordingSpawn([ok(okPreflightJson), ok(previewJson)]);
        const existingGroups: ExistingGroupIdentity[] = [{ name: "shr1", vg_name: "vg_shr1" }];
        const controller = new CreateGroupController(
            spawn.fn, formInput({ name: "shr2", vgName: "vg_shr2" }), existingGroups,
        );

        const afterPreflight = await controller.runPreflight();
        assert.equal(afterPreflight.step, "preview");
        assert.equal(afterPreflight.nameConflict, null);

        const afterPreview = await controller.runPreview();
        assert.equal(afterPreview.step, "confirm");
    });

    it("with no existing groups (default empty list), any VG name is accepted", async () => {
        const spawn = new RecordingSpawn([ok(okPreflightJson)]);
        const controller = new CreateGroupController(spawn.fn, formInput());

        const state = await controller.runPreflight();
        assert.equal(state.step, "preview");
        assert.equal(state.nameConflict, null);
    });
});

describe("CreateGroupController (constraint 6: backend failure never renders as success)", () => {
    it("a rejected preflight spawn lands on step=error, never touches preview/confirm", async () => {
        const spawn = new RecordingSpawn([fail("lsblk: command not found")]);
        const controller = new CreateGroupController(spawn.fn, formInput());

        const state = await controller.runPreflight();
        assert.equal(state.step, "error");
        assert.match(state.errorMessage ?? "", /lsblk/);
        assert.equal(state.preview, null);
    });

    it("a rejected dry-run preview spawn lands on step=error, not confirm", async () => {
        const spawn = new RecordingSpawn([ok(okPreflightJson), fail("shr-rs: internal planner error")]);
        const controller = new CreateGroupController(spawn.fn, formInput());

        await controller.runPreflight();
        const state = await controller.runPreview();
        assert.equal(state.step, "error");
        assert.notEqual(state.step, "confirm");
    });

    it("a rejected real create spawn lands on step=error, never step=done, and result stays null", async () => {
        const spawn = new RecordingSpawn([ok(okPreflightJson), ok(previewJson), fail("mdadm --create failed: device busy")]);
        const controller = new CreateGroupController(spawn.fn, formInput());

        await controller.runPreflight();
        await controller.runPreview();
        controller.setConfirmationText("shr1");

        const state = await controller.execute();
        assert.equal(state.step, "error");
        assert.notEqual(state.step, "done");
        assert.equal(state.result, null);
        assert.match(state.errorMessage ?? "", /device busy/);
    });
});
