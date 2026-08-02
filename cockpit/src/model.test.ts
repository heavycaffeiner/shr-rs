import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
    activeBackgroundSyncs,
    annotateMembers,
    arrayNeedsAttention,
    buildAllocationSegments,
    classifySyncAction,
    computeBandCapacity,
    describeArrayState,
    describeBackgroundActivity,
    describeSyncAction,
    diskMemberHealth,
    formatBytes,
    formatDuration,
    formatScrub,
    formatSyncPercentEta,
    formatSyncProgress,
    groupFaultTolerance,
    groupToleranceStatus,
    parseFsDfOutput,
    parseStatusOutput,
    raidDataMembers,
    raidDisksForBand,
    summarizeAllocation,
    summarizeCapacityUsage,
    summarizeStatus,
    type ArrayStatus,
    type DiskStatus,
    type GroupBandStatus,
    type GroupStatus,
    type MemberStatus,
    type SmartSummary,
} from "./model.ts";
import { installEnglishCatalog } from "./testCatalog.ts";

// Every expectation below is the English wording from `po/en.po`, not a
// message key: see that helper's own comment for why the tests load a real
// catalogue instead of asserting on keys.
installEnglishCatalog();

const validStatus = {
    schema_version: 2,
    health: "degraded",
    disks: [
        {
            name: "sda",
            id: "ata-WD_A",
            size: 4_000_000_000_000,
            model: "Example 4 TB",
            serial: "AAAA",
            rotational: true,
            smart: {
                state: "ok",
                temperature_c: 35,
                power_on_hours: 1200,
                pending_sectors: 0,
                reallocated_sectors: 0,
                uncorrectable_sectors: 0,
                nvme_critical_warning: null,
            },
            arrays: ["md0"],
            system_disk: true,
            system_mounts: ["/", "/boot"],
        },
        {
            name: "sdb",
            id: null,
            size: 6_000_000_000_000,
            model: null,
            serial: null,
            rotational: true,
            smart: {
                state: "warning",
                temperature_c: null,
                power_on_hours: null,
                pending_sectors: 1,
                reallocated_sectors: null,
                uncorrectable_sectors: null,
                nvme_critical_warning: null,
            },
            arrays: [],
            system_disk: false,
            system_mounts: [],
        },
    ],
    arrays: [
        {
            name: "md0",
            level: "raid5",
            state: "active",
            read_only: false,
            degraded: true,
            raid_disks: 3,
            active_disks: 2,
            members: ["sda1", "sdc1"],
            member_states: [
                { name: "sda1", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
                { name: "sdc1", role: 1, faulty: false, spare: false, write_mostly: false, replacement: false },
            ],
            sync: {
                action: "recovery",
                percent: 42.5,
                finish_min: 8.2,
            },
        },
    ],
    groups: [
        {
            name: "shr1",
            mode: "shr",
            layout_version: 2,
            mount_point: "/mnt/shr_data",
            fs_uuid: "11111111-2222-4333-8444-555555555555",
            usable_bytes: 6_000_000_000_000,
            resize_pending: true,
            disks: ["ata-WD_A", "ata-WD_B"],
            vg_name: "shr1_vg",
            lv_name: "data",
            compression: "zstd:3",
            bands: [
                {
                    index: 0,
                    level: "raid1",
                    md_name: "md0",
                    usable_bytes: 4_000_000_000_000,
                    resize_pending: false,
                    members: ["sda1", "sdb1"],
                    member_states: [
                        { name: "sda1", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
                        { name: "sdb1", role: 1, faulty: false, spare: false, write_mostly: false, replacement: false },
                    ],
                    md_uuid: "12345678:9abcdef0:12345678:9abcdef0",
                    sync: null,
                    last_scrub: {
                        finished_at: "2026-07-24T10:15:00Z",
                        outcome: "completed",
                        error_count: 0,
                    },
                    scrub_in_progress: false,
                },
                {
                    index: 1,
                    level: "raid1",
                    md_name: "md1",
                    usable_bytes: 2_000_000_000_000,
                    resize_pending: true,
                    members: ["sdb1", "sdc1"],
                    member_states: [
                        { name: "sdb1", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
                        { name: "sdc1", role: 1, faulty: false, spare: false, write_mostly: false, replacement: false },
                    ],
                    md_uuid: "aaaaaaaa:bbbbbbbb:cccccccc:dddddddd",
                    sync: { action: "resync", percent: 42.5, finish_min: 8.2 },
                    last_scrub: null,
                    scrub_in_progress: false,
                },
            ],
        },
        {
            name: "shr2-hetero",
            mode: "shr2",
            layout_version: 1,
            mount_point: "/mnt/shr2_data",
            fs_uuid: null,
            usable_bytes: 8_000_000_000_000,
            resize_pending: false,
            disks: ["ata-WD_C", "ata-WD_D", "ata-WD_E"],
            vg_name: "shr2_vg",
            lv_name: "data",
            compression: "none",
            bands: [
                {
                    index: 0,
                    level: "raid6",
                    md_name: "md2",
                    usable_bytes: 8_000_000_000_000,
                    resize_pending: false,
                    members: [],
                    member_states: [],
                    md_uuid: null,
                    sync: null,
                    last_scrub: {
                        finished_at: "2026-07-24T09:30:00Z",
                        outcome: "failed",
                        error_count: 3,
                    },
                    scrub_in_progress: true,
                },
            ],
        },
    ],
    state_path: "/var/lib/shr-rs/state.toml",
};

describe("parseStatusOutput", () => {
    it("accepts the schema v2 status contract, including multiple groups", () => {
        const parsed = parseStatusOutput(JSON.stringify(validStatus));
        assert.deepEqual(parsed, validStatus);
        assert.equal(parsed.groups.length, 2);
        assert.equal(parsed.groups[0].mode, "shr");
        assert.equal(parsed.groups[0].resize_pending, true);
        assert.equal(parsed.groups[0].bands[1].resize_pending, true);
        assert.equal(parsed.groups[1].mode, "shr2");
        assert.equal(parsed.groups[1].fs_uuid, null);
    });

    it("defaults members/sync/last_scrub/scrub_in_progress when an older v2 payload omits them", () => {
        // Additive fields from this wave. A payload from a CLI build that
        // predates them (but is still schema_version 2) must still render,
        // not throw -- see GroupBandStatus's doc comment.
        const bareBand = {
            index: 0,
            level: "raid1",
            md_name: "md0",
            usable_bytes: 4_000_000_000_000,
            resize_pending: false,
        };
        const payload = {
            ...validStatus,
            groups: [{ ...validStatus.groups[0], bands: [bareBand] }],
        };
        const parsed = parseStatusOutput(JSON.stringify(payload));
        assert.deepEqual(parsed.groups[0].bands[0], {
            ...bareBand,
            members: [],
            member_states: [],
            md_uuid: null,
            sync: null,
            last_scrub: null,
            scrub_in_progress: false,
        });
    });

    it("defaults an array's member_states to [] when the backend omits it entirely", () => {
        // `member_states` is documented as always-present on the Rust side,
        // but the cockpit contract treats it defensively -- an older CLI
        // build, or any payload that simply doesn't send it, must still
        // render using today's raw `members`-only behavior, not throw.
        const bareArray = {
            name: "md9",
            level: "raid1",
            state: "active",
            read_only: false,
            degraded: false,
            raid_disks: 2,
            active_disks: 2,
            members: ["sdx1", "sdy1"],
            sync: null,
        };
        const parsed = parseStatusOutput(JSON.stringify({ ...validStatus, arrays: [bareArray] }));
        assert.deepEqual(parsed.arrays[0], { ...bareArray, member_states: [] });
    });

    it("parses id/system_disk/system_mounts when the backend sends them", () => {
        const parsed = parseStatusOutput(JSON.stringify(validStatus));
        assert.equal(parsed.disks[0].id, "ata-WD_A");
        assert.equal(parsed.disks[0].system_disk, true);
        assert.deepEqual(parsed.disks[0].system_mounts, ["/", "/boot"]);
        assert.equal(parsed.disks[1].id, null);
        assert.equal(parsed.disks[1].system_disk, false);
    });

    it("defaults id/system_disk/system_mounts when the backend omits them entirely", () => {
        // Rust conditionally serializes `id`/`system_mounts` (skip_serializing_if)
        // and `system_disk` is `#[serde(default)]` -- all three can be absent
        // from a real payload, not just present-and-null. Absent must default,
        // not throw (same "additive field" rule as the band fields above).
        const bareDisk = {
            name: "sdz",
            size: 1_000_000_000_000,
            model: null,
            serial: null,
            rotational: true,
            smart: validStatus.disks[0].smart,
            arrays: [],
        };
        const parsed = parseStatusOutput(JSON.stringify({ ...validStatus, disks: [bareDisk] }));
        assert.deepEqual(parsed.disks[0], { ...bareDisk, id: null, system_disk: false, system_mounts: [] });
    });

    it("parses a faulty member's state on both an array and its band", () => {
        const faultyState: MemberStatus = {
            name: "loop12p1", role: 3, faulty: true, spare: false, write_mostly: false, replacement: false,
        };
        const payload = {
            ...validStatus,
            arrays: [{
                ...validStatus.arrays[0],
                members: [...validStatus.arrays[0].members, "loop12p1"],
                member_states: [...validStatus.arrays[0].member_states, faultyState],
            }],
            groups: [{
                ...validStatus.groups[0],
                bands: [{
                    ...validStatus.groups[0].bands[0],
                    members: [...validStatus.groups[0].bands[0].members, "loop12p1"],
                    member_states: [...validStatus.groups[0].bands[0].member_states, faultyState],
                }, validStatus.groups[0].bands[1]],
            }],
        };
        const parsed = parseStatusOutput(JSON.stringify(payload));
        const arrayFaulty = parsed.arrays[0].member_states?.find(m => m.name === "loop12p1");
        assert.equal(arrayFaulty?.faulty, true);
        assert.equal(arrayFaulty?.role, 3);
        const bandFaulty = parsed.groups[0].bands[0].member_states?.find(m => m.name === "loop12p1");
        assert.equal(bandFaulty?.faulty, true);
    });

    it("parses a spare member's state, distinct from faulty", () => {
        const spareState: MemberStatus = {
            name: "sde1", role: 4, faulty: false, spare: true, write_mostly: false, replacement: false,
        };
        const payload = {
            ...validStatus,
            arrays: [{
                ...validStatus.arrays[0],
                members: [...validStatus.arrays[0].members, "sde1"],
                member_states: [...validStatus.arrays[0].member_states, spareState],
            }],
        };
        const parsed = parseStatusOutput(JSON.stringify(payload));
        const spare = parsed.arrays[0].member_states?.find(m => m.name === "sde1");
        assert.equal(spare?.spare, true);
        assert.equal(spare?.faulty, false);
    });

    it("parses md_uuid/vg_name/lv_name/compression when the backend sends them", () => {
        const parsed = parseStatusOutput(JSON.stringify(validStatus));
        assert.equal(parsed.groups[0].bands[0].md_uuid, "12345678:9abcdef0:12345678:9abcdef0");
        assert.equal(parsed.groups[0].vg_name, "shr1_vg");
        assert.equal(parsed.groups[0].lv_name, "data");
        assert.equal(parsed.groups[0].compression, "zstd:3");
    });

    it("defaults a band's md_uuid to null when the backend omits it (unassembled band)", () => {
        const bandWithoutUuid = { ...validStatus.groups[0].bands[0] };
        delete (bandWithoutUuid as Record<string, unknown>).md_uuid;
        const parsed = parseStatusOutput(JSON.stringify({
            ...validStatus,
            groups: [{ ...validStatus.groups[0], bands: [bandWithoutUuid, validStatus.groups[0].bands[1]] }],
        }));
        assert.equal(parsed.groups[0].bands[0].md_uuid, null);
    });

    it("parses state_path when the backend sends it", () => {
        const parsed = parseStatusOutput(JSON.stringify(validStatus));
        assert.equal(parsed.state_path, "/var/lib/shr-rs/state.toml");
    });

    it("defaults state_path to null when the backend omits it -- a v2 payload from before that fix", () => {
        const payload = { ...validStatus };
        delete (payload as Record<string, unknown>).state_path;
        const parsed = parseStatusOutput(JSON.stringify(payload));
        assert.equal(parsed.state_path, null);
    });

    it("rejects a non-string state_path instead of rendering garbage", () => {
        assert.throws(
            () => parseStatusOutput(JSON.stringify({ ...validStatus, state_path: 42 })),
            /configuration file path/,
        );
    });

    it("rejects a group missing vg_name/lv_name/compression instead of rendering \"Unknown\" silently", () => {
        const groupWithoutVgName = { ...validStatus.groups[0] };
        delete (groupWithoutVgName as Record<string, unknown>).vg_name;
        assert.throws(
            () => parseStatusOutput(JSON.stringify({ ...validStatus, groups: [groupWithoutVgName] })),
            /the volume group name/,
        );
    });

    it("rejects a malformed scrub outcome instead of rendering garbage", () => {
        assert.throws(
            () => parseStatusOutput(JSON.stringify({
                ...validStatus,
                groups: [{
                    ...validStatus.groups[0],
                    bands: [{
                        ...validStatus.groups[0].bands[0],
                        last_scrub: { finished_at: "2026-01-01T00:00:00Z", outcome: "exploded", error_count: 0 },
                    }],
                }],
            })),
            /the scrub outcome/,
        );
    });

    it("rejects malformed JSON, CLI error objects, and unknown schemas", () => {
        assert.throws(() => parseStatusOutput("not json"), /valid JSON status output/);
        assert.throws(
            () => parseStatusOutput(JSON.stringify({ error: "lsblk failed" })),
            /lsblk failed/,
        );
        assert.throws(
            () => parseStatusOutput(JSON.stringify({ ...validStatus, schema_version: 3 })),
            /not compatible with the dashboard/,
        );
    });

    it("rejects the old schema v1 contract outright -- the contract genuinely changed", () => {
        // A pre-multi-group shr-rs binary's status output: no `groups` key
        // at all, schema_version 1. This must fail loudly, not be silently
        // treated as "zero groups" (see report.rs's SCHEMA_VERSION doc
        // comment for why those two cases must never be confused).
        const v1 = { ...validStatus, schema_version: 1 } as Record<string, unknown>;
        delete v1.groups;
        assert.throws(
            () => parseStatusOutput(JSON.stringify(v1)),
            /not compatible with the dashboard/,
        );
    });

    it("rejects structurally incomplete reports before rendering", () => {
        assert.throws(
            () => parseStatusOutput(JSON.stringify({ ...validStatus, disks: [{}] })),
            /Disk entry #/,
        );
    });

    it("rejects malformed group entries instead of rendering garbage", () => {
        assert.throws(
            () => parseStatusOutput(JSON.stringify({ ...validStatus, groups: [{}] })),
            /Group entry #/,
        );
        assert.throws(
            () => parseStatusOutput(JSON.stringify({
                ...validStatus,
                groups: [{ ...validStatus.groups[0], mode: 42 }],
            })),
            /the mode/,
        );
        assert.throws(
            () => parseStatusOutput(JSON.stringify({
                ...validStatus,
                groups: [{ ...validStatus.groups[0], usable_bytes: -1 }],
            })),
            /the usable capacity/,
        );
        assert.throws(
            () => parseStatusOutput(JSON.stringify({
                ...validStatus,
                groups: [{
                    ...validStatus.groups[0],
                    bands: [{ ...validStatus.groups[0].bands[0], resize_pending: "yes" }],
                }],
            })),
            /the pending-expansion flag/,
        );
        assert.throws(
            () => parseStatusOutput(JSON.stringify({
                ...validStatus,
                groups: [{ ...validStatus.groups[0], bands: "not-an-array" }],
            })),
            /the band list/,
        );
    });
});

describe("summarizeStatus", () => {
    it("derives honest observed-capacity and health totals", () => {
        const summary = summarizeStatus(parseStatusOutput(JSON.stringify(validStatus)));

        assert.deepEqual(summary, {
            rawBytes: 10_000_000_000_000,
            unknownSizeDisks: 0,
            linkedDisks: 1,
            unlinkedDisks: 1,
            systemDisks: 1,
            warningDisks: 1,
            warningArrays: 1,
            activeMembers: 2,
            expectedMembers: 3,
        });
    });

    it("never counts a system disk as RAID-unlinked/spare capacity", () => {
        // disks[0] is `system_disk: true` but also happens to sit in an
        // array in this fixture; add a second system disk that is NOT in
        // any array, mirroring the real defect (/dev/vda: system disk, no
        // array membership) -- that disk must land in `systemDisks`, never
        // inflate `unlinkedDisks`.
        const report = parseStatusOutput(JSON.stringify({
            ...validStatus,
            disks: [
                ...validStatus.disks,
                {
                    ...validStatus.disks[1],
                    name: "vda",
                    size: 25_800_000_000,
                    arrays: [],
                    system_disk: true,
                    system_mounts: ["/", "/boot", "/boot/efi"],
                },
            ],
        }));
        const summary = summarizeStatus(report);
        assert.equal(summary.systemDisks, 2);
        // Only sdb (non-system, no array) counts as spare/unlinked; vda does not.
        assert.equal(summary.unlinkedDisks, 1);
        assert.equal(summary.linkedDisks, 1);
    });

    it("does not invent byte totals when a physical disk size is unknown", () => {
        const report = parseStatusOutput(JSON.stringify({
            ...validStatus,
            disks: [
                { ...validStatus.disks[0], size: null },
                validStatus.disks[1],
            ],
        }));

        const summary = summarizeStatus(report);
        assert.equal(summary.rawBytes, null);
        assert.equal(summary.unknownSizeDisks, 1);
    });

    it("counts inactive arrays as warnings even when mdstat does not mark them degraded", () => {
        const inactive = {
            ...validStatus.arrays[0],
            state: "inactive",
            degraded: false,
            active_disks: 3,
        };
        const report = parseStatusOutput(JSON.stringify({
            ...validStatus,
            arrays: [inactive],
        }));

        assert.equal(arrayNeedsAttention(report.arrays[0]), true);
        assert.equal(summarizeStatus(report).warningArrays, 1);
    });

    it("uses the same warning rule for state, read-only, degraded, and invalid RAID6 arrays", () => {
        const healthy = parseStatusOutput(JSON.stringify(validStatus)).arrays[0];
        healthy.degraded = false;
        healthy.active_disks = healthy.raid_disks;
        assert.equal(arrayNeedsAttention(healthy), false);
        assert.equal(arrayNeedsAttention({ ...healthy, state: "clean" }), false);
        assert.equal(arrayNeedsAttention({ ...healthy, state: "inactive" }), true);
        assert.equal(arrayNeedsAttention({ ...healthy, read_only: true }), true);
        assert.equal(arrayNeedsAttention({ ...healthy, degraded: true }), true);
        assert.equal(arrayNeedsAttention({
            ...healthy,
            level: "raid6",
            raid_disks: 3,
        }), true);
    });
});

describe("formatBytes", () => {
    it("uses decimal storage units and preserves unknown values", () => {
        assert.equal(formatBytes(4_000_000_000_000), "4.0 TB");
        assert.equal(formatBytes(512_000_000_000), "512.0 GB");
        assert.equal(formatBytes(0), "0 B");
        assert.equal(formatBytes(null), "Unknown");
    });
});

const makeBand = (overrides: Partial<GroupBandStatus>): GroupBandStatus => ({
    index: 0,
    level: "raid6",
    md_name: "md0",
    usable_bytes: 0,
    resize_pending: false,
    members: [],
    sync: null,
    last_scrub: null,
    scrub_in_progress: false,
    ...overrides,
});

describe("raidDataMembers", () => {
    it("mirrors RaidLevel::data_members for the three shr-rs levels", () => {
        assert.equal(raidDataMembers("raid1", 2), 1);
        assert.equal(raidDataMembers("raid1", 5), 1);
        assert.equal(raidDataMembers("raid5", 3), 2);
        assert.equal(raidDataMembers("raid5", 6), 5);
        assert.equal(raidDataMembers("raid6", 4), 2);
        assert.equal(raidDataMembers("raid6", 5), 3);
    });

    it("returns null for an unrecognized level or too few members to be redundant", () => {
        assert.equal(raidDataMembers("raid0", 4), null);
        assert.equal(raidDataMembers("linear", 1), null);
        assert.equal(raidDataMembers("raid5", 1), null);
        assert.equal(raidDataMembers("raid6", 2), null);
        assert.equal(raidDataMembers("raid1", 0), null);
    });
});

const makeArray = (overrides: Partial<ArrayStatus>): ArrayStatus => ({
    name: "md0",
    level: "raid5",
    state: "active",
    read_only: false,
    degraded: false,
    raid_disks: null,
    active_disks: null,
    members: [],
    member_states: [],
    sync: null,
    ...overrides,
});

describe("computeBandCapacity", () => {
    it("matches the mockup's band0 worked example (RAID6, raid_disks=5, 12/20 TB)", () => {
        const band = makeBand({ level: "raid6", usable_bytes: 12_000_000_000_000 });
        const capacity = computeBandCapacity(band, 5);
        assert.ok(capacity);
        assert.equal(capacity.memberCount, 5);
        assert.equal(capacity.dataMembers, 3);
        assert.equal(capacity.memberBytes, 4_000_000_000_000);
        assert.equal(capacity.parityBytes, 8_000_000_000_000);
        assert.equal(capacity.rawBytes, 20_000_000_000_000);
    });

    it("returns null when raid_disks isn't known (array not currently live/assembled)", () => {
        assert.equal(computeBandCapacity(makeBand({}), null), null);
    });

    it("returns null when raid_disks can't back the reported level (e.g. below the level's minimum)", () => {
        assert.equal(computeBandCapacity(makeBand({ level: "raid6" }), 1), null);
    });

    // A correction to that same fix, which made faulty members
    // visible but wrongly fed the LIVE member count into slice/total math --
    // first counting a stale faulty member as data (earlier bug), then, after
    // the (also wrong) earlier fix, excluding it and shrinking the total. Real
    // guest measurement (md0/raid5, group `ga`, `loop11p1` failed via
    // `mdadm --fail`): raid_disks=3, usable_bytes=8_589_934_592 (8.6 GB),
    // live members = [loop12p1, loop11p1(F), loop10p1]. A disk failing does
    // not change how many disk-slots the array occupies -- only raid_disks
    // (the array's configured geometry) may drive this math.
    it("derives slice size and total from raid_disks, not from the live member list (real-guest repro)", () => {
        const band = makeBand({ level: "raid5", usable_bytes: 8_589_934_592 });
        const capacity = computeBandCapacity(band, 3);
        assert.ok(capacity);
        assert.equal(capacity.memberCount, 3);
        assert.equal(capacity.dataMembers, 2);
        assert.equal(capacity.memberBytes, 4_294_967_296); // 4.3 GB (8.6 GB / 2)
        assert.equal(capacity.parityBytes, 4_294_967_296);
        assert.equal(capacity.rawBytes, 12_884_901_888); // 12.9 GB (3 x 4.3 GB)
    });

    it("gives the identical result whether or not a live member is faulty -- geometry doesn't change", () => {
        const allHealthy = makeBand({
            level: "raid5",
            usable_bytes: 8_589_934_592,
            members: ["loop12p1", "loop11p1", "loop10p1"],
            member_states: [
                { name: "loop12p1", role: 3, faulty: false, spare: false, write_mostly: false, replacement: false },
                { name: "loop11p1", role: 1, faulty: false, spare: false, write_mostly: false, replacement: false },
                { name: "loop10p1", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
            ],
        });
        const oneFaulty = makeBand({
            level: "raid5",
            usable_bytes: 8_589_934_592,
            members: ["loop12p1", "loop11p1", "loop10p1"],
            member_states: [
                { name: "loop12p1", role: 3, faulty: false, spare: false, write_mostly: false, replacement: false },
                { name: "loop11p1", role: 1, faulty: true, spare: false, write_mostly: false, replacement: false },
                { name: "loop10p1", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
            ],
        });
        // Same raid_disks (3) either way -- a member failing must not change
        // the result at all, which is the whole point of this correction.
        assert.deepEqual(computeBandCapacity(allHealthy, 3), computeBandCapacity(oneFaulty, 3));
        assert.equal(computeBandCapacity(oneFaulty, 3)?.rawBytes, 12_884_901_888);
    });

    it("uses raid_disks even when a stale replace-residual member makes the live list LONGER than raid_disks", () => {
        // A `--replace`d member can stay attached (faulty) until its
        // background copy finishes and periodic reconciliation removes it,
        // so `members`/`member_states` can briefly list MORE entries than
        // `raid_disks`. Geometry must still follow raid_disks (3), not the
        // live count (4).
        const band = makeBand({
            level: "raid5",
            usable_bytes: 8_589_934_592,
            members: ["loop14p1", "loop12p1", "loop11p1", "loop10p1"],
            member_states: [
                { name: "loop14p1", role: 4, faulty: false, spare: false, write_mostly: false, replacement: false },
                { name: "loop12p1", role: 3, faulty: true, spare: false, write_mostly: false, replacement: false },
                { name: "loop11p1", role: 1, faulty: false, spare: false, write_mostly: false, replacement: false },
                { name: "loop10p1", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
            ],
        });
        const capacity = computeBandCapacity(band, raidDisksForBand(band, [
            makeArray({ name: "md0", raid_disks: 3 }),
        ]));
        assert.ok(capacity);
        assert.equal(capacity.memberCount, 3);
        assert.equal(capacity.memberBytes, 4_294_967_296);
        assert.equal(capacity.rawBytes, 12_884_901_888);
    });
});

describe("raidDisksForBand", () => {
    it("reads raid_disks from the array correlated by md_name", () => {
        const band = makeBand({ md_name: "md0" });
        const arrays = [makeArray({ name: "md0", raid_disks: 3 }), makeArray({ name: "md1", raid_disks: 5 })];
        assert.equal(raidDisksForBand(band, arrays), 3);
    });

    it("returns null (never invents a number) when no live array matches the band's md_name", () => {
        const band = makeBand({ md_name: "md9" });
        assert.equal(raidDisksForBand(band, [makeArray({ name: "md0", raid_disks: 3 })]), null);
    });

    it("returns null when the correlated array's own raid_disks is null", () => {
        const band = makeBand({ md_name: "md0" });
        assert.equal(raidDisksForBand(band, [makeArray({ name: "md0", raid_disks: null })]), null);
    });
});

describe("annotateMembers", () => {
    it("pairs each member name with its faulty/spare flag", () => {
        const states: MemberStatus[] = [
            { name: "a", role: 0, faulty: true, spare: false, write_mostly: false, replacement: false },
            { name: "b", role: 1, faulty: false, spare: true, write_mostly: false, replacement: false },
        ];
        assert.deepEqual(annotateMembers(["a", "b", "c"], states), [
            { name: "a", faulty: true, spare: false, write_mostly: false, replacement: false },
            { name: "b", faulty: false, spare: true, write_mostly: false, replacement: false },
            { name: "c", faulty: false, spare: false, write_mostly: false, replacement: false },
        ]);
    });

    it("treats every member as healthy when member_states is undefined", () => {
        assert.deepEqual(
            annotateMembers(["a"], undefined),
            [{ name: "a", faulty: false, spare: false, write_mostly: false, replacement: false }],
        );
    });

    // Write_mostly/replacement were parsed into MemberStatus but dropped
    // here, so a member mid-`disk replace` read identically to a plain spare.
    it("carries write_mostly and replacement through, not just faulty/spare", () => {
        const states: MemberStatus[] = [
            { name: "a", role: 0, faulty: false, spare: false, write_mostly: true, replacement: false },
            { name: "b", role: 1, faulty: false, spare: true, write_mostly: false, replacement: true },
        ];
        assert.deepEqual(annotateMembers(["a", "b"], states), [
            { name: "a", faulty: false, spare: false, write_mostly: true, replacement: false },
            { name: "b", faulty: false, spare: true, write_mostly: false, replacement: true },
        ]);
    });
});

describe("diskMemberHealth", () => {
    const baseSmart: SmartSummary = {
        state: "ok",
        temperature_c: null,
        power_on_hours: null,
        pending_sectors: null,
        reallocated_sectors: null,
        uncorrectable_sectors: null,
        nvme_critical_warning: null,
    };
    const makeDisk = (name: string, arrays: string[]): DiskStatus => (
        { name, size: null, model: null, serial: null, rotational: null, smart: baseSmart, arrays }
    );
    const makeArray = (name: string, memberStates: MemberStatus[]): ArrayStatus => ({
        name,
        level: "raid5",
        state: "active",
        read_only: false,
        degraded: false,
        raid_disks: null,
        active_disks: null,
        members: memberStates.map(m => m.name),
        member_states: memberStates,
        sync: null,
    });

    it("flags a disk whose partition is a faulty array member", () => {
        const arrays = [makeArray("md0", [
            { name: "sdc1", role: 2, faulty: true, spare: false, write_mostly: false, replacement: false },
        ])];
        assert.deepEqual(
            diskMemberHealth(makeDisk("sdc", ["md0"]), arrays),
            { faulty: true, spare: false, write_mostly: false, replacement: false },
        );
    });

    it("flags a disk whose partition is a spare, distinctly from faulty", () => {
        const arrays = [makeArray("md0", [
            { name: "sde1", role: 4, faulty: false, spare: true, write_mostly: false, replacement: false },
        ])];
        assert.deepEqual(
            diskMemberHealth(makeDisk("sde", ["md0"]), arrays),
            { faulty: false, spare: true, write_mostly: false, replacement: false },
        );
    });

    it("matches loop/nvme-style partition names (diskNamep1)", () => {
        const arrays = [makeArray("md0", [
            { name: "loop12p1", role: 3, faulty: true, spare: false, write_mostly: false, replacement: false },
        ])];
        assert.deepEqual(
            diskMemberHealth(makeDisk("loop12", ["md0"]), arrays),
            { faulty: true, spare: false, write_mostly: false, replacement: false },
        );
    });

    it("reads healthy when the member isn't faulty, and when the named array can't be found", () => {
        const arrays = [makeArray("md0", [
            { name: "sdb1", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
        ])];
        assert.deepEqual(
            diskMemberHealth(makeDisk("sdb", ["md0"]), arrays),
            { faulty: false, spare: false, write_mostly: false, replacement: false },
        );
        assert.deepEqual(
            diskMemberHealth(makeDisk("sdz", ["md9"]), arrays),
            { faulty: false, spare: false, write_mostly: false, replacement: false },
        );
    });

    // A member mid-`disk replace` carries `replacement: true` in live
    // mdstat -- must not disappear on the way to the drive-list chip.
    it("flags a disk whose partition is a live replacement target, distinctly from spare", () => {
        const arrays = [makeArray("md0", [
            { name: "sdf1", role: 2, faulty: false, spare: false, write_mostly: false, replacement: true },
        ])];
        assert.deepEqual(
            diskMemberHealth(makeDisk("sdf", ["md0"]), arrays),
            { faulty: false, spare: false, write_mostly: false, replacement: true },
        );
    });
});

describe("summarizeAllocation", () => {
    it("sums usable bytes always, and parity/raw only from bands correlated to a live array", () => {
        const report = parseStatusOutput(JSON.stringify(validStatus));
        const allocation = summarizeAllocation(report);

        // Both groups' usable_bytes: 4T + 2T (shr1) + 8T (shr2-hetero).
        assert.equal(allocation.usableBytes, 14_000_000_000_000);

        // Only shr1's band0 (md_name "md0") correlates to `validStatus`'s
        // one live array; band1 ("md1") and shr2-hetero's band ("md2") have
        // no matching live array, so they must not silently contribute (or
        // silently be treated as zero).
        assert.equal(allocation.parityBytesPartial, true);
        assert.ok(allocation.parityBytes !== null && allocation.parityBytes > 0);
        assert.equal(allocation.bandRawBytes, null); // not ALL bands are live
        // disks[0] (sda) is the fixture's system disk.
        assert.equal(allocation.systemDiskBytes, 4_000_000_000_000);
    });

    it("reports null (never a fabricated raw total) when no array is live -- no raid_disks to derive geometry from", () => {
        // Mirrors reality: `report.arrays` only ever lists currently-live
        // arrays (see `raidDisksForBand`'s doc comment), so "no band is
        // live" means an empty `arrays` list, not just empty `members`.
        const report = parseStatusOutput(JSON.stringify({
            ...validStatus,
            arrays: [],
            groups: [{
                ...validStatus.groups[0],
                bands: validStatus.groups[0].bands.map(band => ({ ...band, members: [] })),
            }],
        }));
        const allocation = summarizeAllocation(report);
        assert.equal(allocation.parityBytes, null);
        assert.equal(allocation.parityBytesPartial, false);
        assert.equal(allocation.bandRawBytes, null);
        assert.equal(allocation.unassignedBytes, null);
    });
});

// A system disk's (and any disk holding it) bytes must never be
// readable as "unassigned/addable" capacity -- shr-rs preflight refuses a
// system disk outright, so the dashboard must not disagree with it.
describe("summarizeAllocation -- system disk exclusion", () => {
    // One live raid1 band (2 non-system members, mirrored) plus a spare
    // non-system disk plus a system disk that is in no array at all --
    // mirrors the real defect (/dev/vda: system disk, 4 disks total,
    // 2 in a live band, 1 genuinely spare).
    const makeReport = (disks: DiskStatus[]) => parseStatusOutput(JSON.stringify({
        ...validStatus,
        disks,
        // Capacity math now reads `raid_disks` from the correlated
        // (by `md_name`) live array, not from the band's own member list --
        // this must define its own 2-disk raid1 array rather than inherit
        // validStatus.arrays' unrelated 3-disk raid5 "md0".
        arrays: [{
            name: "md0",
            level: "raid1",
            state: "active",
            read_only: false,
            degraded: false,
            raid_disks: 2,
            active_disks: 2,
            members: ["data-a1", "data-b1"],
            sync: null,
        }],
        groups: [{
            ...validStatus.groups[0],
            bands: [{
                index: 0,
                level: "raid1",
                md_name: "md0",
                usable_bytes: 3_000_000_000_000,
                resize_pending: false,
                members: ["data-a1", "data-b1"],
                sync: null,
                last_scrub: null,
                scrub_in_progress: false,
            }],
        }],
    }));

    const baseSmart: SmartSummary = { ...validStatus.disks[0].smart, state: "ok" };
    const systemDisk: DiskStatus = {
        name: "vda",
        id: "ata-SYS",
        size: 5_000_000_000_000,
        model: null,
        serial: null,
        rotational: true,
        smart: baseSmart,
        arrays: [],
        system_disk: true,
        system_mounts: ["/", "/boot"],
    };
    const dataDiskA: DiskStatus = {
        name: "vdc",
        id: "ata-A",
        size: 3_000_000_000_000,
        model: null,
        serial: null,
        rotational: true,
        smart: baseSmart,
        arrays: ["md0"],
        system_disk: false,
        system_mounts: [],
    };
    const dataDiskB: DiskStatus = {
        name: "vdd",
        id: "ata-B",
        size: 3_000_000_000_000,
        model: null,
        serial: null,
        rotational: true,
        smart: baseSmart,
        arrays: ["md0"],
        system_disk: false,
        system_mounts: [],
    };
    const spareDisk: DiskStatus = {
        name: "vde",
        id: "ata-SPARE",
        size: 2_000_000_000_000,
        model: null,
        serial: null,
        rotational: true,
        smart: baseSmart,
        arrays: [],
        system_disk: false,
        system_mounts: [],
    };

    it("excludes the system disk's bytes from unassignedBytes when a system disk is present", () => {
        const allocation = summarizeAllocation(makeReport([systemDisk, dataDiskA, dataDiskB, spareDisk]));
        assert.equal(allocation.systemDiskBytes, 5_000_000_000_000);
        assert.equal(allocation.bandRawBytes, 6_000_000_000_000); // raid1: 3T usable + 3T mirror
        // Only the spare (non-system, unbanded) disk's 2T counts as unassigned
        // -- the system disk's 5T must NOT be added in, even though it is
        // also outside any band.
        assert.equal(allocation.unassignedBytes, 2_000_000_000_000);
        assert.equal(allocation.rawDiskBytes, 13_000_000_000_000); // all 4 disks, for reference

        const segments = buildAllocationSegments(allocation, { usedBytes: null, freeBytes: null });
        const unassigned = segments.find(s => s.kind === "unassigned");
        const system = segments.find(s => s.kind === "system");
        assert.equal(unassigned?.bytes, 2_000_000_000_000);
        assert.equal(system?.bytes, 5_000_000_000_000);
    });

    it("behaves exactly as before (systemDiskBytes 0) when no disk is a system disk", () => {
        const allocation = summarizeAllocation(makeReport([dataDiskA, dataDiskB, spareDisk]));
        assert.equal(allocation.systemDiskBytes, 0);
        assert.equal(allocation.unassignedBytes, 2_000_000_000_000);

        const segments = buildAllocationSegments(allocation, { usedBytes: null, freeBytes: null });
        assert.equal(segments.find(s => s.kind === "system"), undefined);
    });

    it("never fabricates a sum when a disk's size is unknown -- unknown disk poisons only its own bucket", () => {
        // A non-system disk with an unknown size must null out unassignedBytes
        // (same "unknown poisons the sum" rule as everywhere else) without
        // touching systemDiskBytes, which is computed from a disjoint set.
        const unknownSpare = { ...spareDisk, size: null };
        const withUnknownSpare = summarizeAllocation(makeReport([systemDisk, dataDiskA, dataDiskB, unknownSpare]));
        assert.equal(withUnknownSpare.unassignedBytes, null);
        assert.equal(withUnknownSpare.systemDiskBytes, 5_000_000_000_000);

        // Symmetrically, an unknown-size SYSTEM disk must null out
        // systemDiskBytes without blocking unassignedBytes from being
        // computed from the (fully known) non-system disks.
        const unknownSystem = { ...systemDisk, size: null };
        const withUnknownSystem = summarizeAllocation(makeReport([unknownSystem, dataDiskA, dataDiskB, spareDisk]));
        assert.equal(withUnknownSystem.systemDiskBytes, null);
        assert.equal(withUnknownSystem.unassignedBytes, 2_000_000_000_000);
    });
});

describe("summarizeCapacityUsage", () => {
    it("returns null used/free when fs df was never fetched or has no groups", () => {
        assert.deepEqual(summarizeCapacityUsage(null, 1_000), { usedBytes: null, freeBytes: null });
        assert.deepEqual(
            summarizeCapacityUsage({ schema_version: 2, groups: [] }, 1_000),
            { usedBytes: null, freeBytes: null },
        );
    });

    it("returns null (today's honest reality) when fs df has no live Btrfs usage parser output", () => {
        // Mirrors `fs_df_report` always calling `build_fs_df` with an empty
        // usage map -- every group's used/total bytes come back `null`.
        const fsDf = {
            schema_version: 2 as const,
            groups: [{
                name: "shr1",
                mount_point: "/mnt/shr_data",
                usable_bytes: 6_000_000_000_000,
                data_used_bytes: null,
                data_total_bytes: null,
                metadata_used_bytes: null,
                metadata_total_bytes: null,
                unallocated_bytes: null,
                statvfs_avail_bytes: null,
            }],
        };
        assert.deepEqual(summarizeCapacityUsage(fsDf, 6_000_000_000_000), { usedBytes: null, freeBytes: null });
    });

    it("sums used bytes and derives free bytes once live usage IS known for every group", () => {
        const fsDf = {
            schema_version: 2 as const,
            groups: [{
                name: "shr1",
                mount_point: "/mnt/shr_data",
                usable_bytes: 6_000_000_000_000,
                data_used_bytes: 4_000_000_000_000,
                data_total_bytes: 5_000_000_000_000,
                metadata_used_bytes: 200_000_000_000,
                metadata_total_bytes: 300_000_000_000,
                unallocated_bytes: 700_000_000_000,
                statvfs_avail_bytes: 1_800_000_000_000,
            }],
        };
        assert.deepEqual(
            summarizeCapacityUsage(fsDf, 6_000_000_000_000),
            { usedBytes: 4_200_000_000_000, freeBytes: 1_800_000_000_000 },
        );
    });

    it("never sums a partial reading -- one group known and one unknown must still report null", () => {
        const fsDf = {
            schema_version: 2 as const,
            groups: [
                {
                    name: "g1",
                    mount_point: "/mnt/g1",
                    usable_bytes: 1_000,
                    data_used_bytes: 500,
                    data_total_bytes: 800,
                    metadata_used_bytes: 10,
                    metadata_total_bytes: 20,
                    unallocated_bytes: 200,
                    statvfs_avail_bytes: 400,
                },
                {
                    name: "g2",
                    mount_point: "/mnt/g2",
                    usable_bytes: 2_000,
                    data_used_bytes: null,
                    data_total_bytes: null,
                    metadata_used_bytes: null,
                    metadata_total_bytes: null,
                    unallocated_bytes: null,
                    statvfs_avail_bytes: null,
                },
            ],
        };
        assert.deepEqual(summarizeCapacityUsage(fsDf, 3_000), { usedBytes: null, freeBytes: null });
    });
});

describe("buildAllocationSegments", () => {
    it("collapses used/free into a single unknown segment when usage isn't measured", () => {
        const segments = buildAllocationSegments(
            {
                usableBytes: 14_000_000_000_000,
                parityBytes: 8_000_000_000_000,
                parityBytesPartial: false,
                rawDiskBytes: 28_000_000_000_000,
                bandRawBytes: 20_000_000_000_000,
                systemDiskBytes: 0,
                unassignedBytes: 4_000_000_000_000,
            },
            { usedBytes: null, freeBytes: null },
        );
        assert.deepEqual(segments, [
            { kind: "unknown", bytes: 14_000_000_000_000 },
            { kind: "parity", bytes: 8_000_000_000_000 },
            { kind: "unassigned", bytes: 4_000_000_000_000 },
        ]);
    });

    it("splits into used/free once both are known, and omits zero-byte segments", () => {
        const segments = buildAllocationSegments(
            {
                usableBytes: 6_000_000_000_000,
                parityBytes: null,
                parityBytesPartial: false,
                rawDiskBytes: null,
                bandRawBytes: null,
                systemDiskBytes: null,
                unassignedBytes: null,
            },
            { usedBytes: 4_000_000_000_000, freeBytes: 2_000_000_000_000 },
        );
        assert.deepEqual(segments, [
            { kind: "used", bytes: 4_000_000_000_000 },
            { kind: "free", bytes: 2_000_000_000_000 },
        ]);
    });
});

describe("groupFaultTolerance", () => {
    it("maps shr/shr2 to their guaranteed disk-loss tolerance and leaves unknown modes null", () => {
        assert.equal(groupFaultTolerance("shr"), 1);
        assert.equal(groupFaultTolerance("SHR2"), 2);
        assert.equal(groupFaultTolerance("mystery-mode"), null);
    });
});

// The fault-tolerance card showed the mode's nominal tolerance
// unconditionally, so a group already missing a disk read identically to a
// fully healthy one. `groupToleranceStatus` must subtract live faulty-member
// counts from the nominal figure, driven by whichever band is worst (bands
// can differ in width/level), and must fall back to "unknown" -- never
// "healthy" -- the moment any band's live member state isn't available,
// since that band could be the one that's actually degraded.
describe("groupToleranceStatus", () => {
    const member = (name: string, faulty: boolean): MemberStatus => (
        { name, role: 0, faulty, spare: false, write_mostly: false, replacement: false }
    );

    const band = (index: number, memberStates: MemberStatus[] | undefined): GroupBandStatus => ({
        index,
        level: "raid5",
        md_name: `md${index}`,
        usable_bytes: 1,
        resize_pending: false,
        members: (memberStates ?? []).map(m => m.name),
        // Omit the key entirely (not `member_states: undefined`) to match
        // "band never went live" -- exactOptionalPropertyTypes forbids the
        // literal undefined value on an optional field.
        ...(memberStates !== undefined ? { member_states: memberStates } : {}),
        sync: null,
        last_scrub: null,
        scrub_in_progress: false,
    });

    it("a fully healthy shr2 group reports remaining equal to nominal", () => {
        const bands = [
            band(0, [member("sda1", false), member("sdb1", false)]),
            band(1, [member("sdc1", false), member("sdd1", false)]),
        ];
        assert.deepEqual(groupToleranceStatus("shr2", bands), { nominal: 2, remaining: 2 });
    });

    it("one faulty member in one band reduces remaining by exactly that band's faulty count", () => {
        const bands = [
            band(0, [member("sda1", true), member("sdb1", false)]),
            band(1, [member("sdc1", false), member("sdd1", false)]),
        ];
        assert.deepEqual(groupToleranceStatus("shr2", bands), { nominal: 2, remaining: 1 });
    });

    it("the worst band drives remaining, not an average, across bands of different width/level", () => {
        const bands = [
            band(0, [member("sda1", false), member("sdb1", false)]), // raid1, 0 faulty
            band(1, [
                member("sdc1", true), member("sdd1", true), member("sde1", false), member("sdf1", false),
            ]), // raid6, 2 faulty
        ];
        assert.deepEqual(groupToleranceStatus("shr2", bands), { nominal: 2, remaining: 0 });
    });

    it("goes negative, not clamped at zero, when a band already lost more members than the mode ever promised", () => {
        const bands = [band(0, [member("sda1", true), member("sdb1", true), member("sdc1", false)])];
        assert.deepEqual(groupToleranceStatus("shr", bands), { nominal: 1, remaining: -1 });
    });

    it("reports remaining as unknown, not healthy, when a band has no live member-state info at all", () => {
        assert.deepEqual(groupToleranceStatus("shr", [band(0, undefined)]), { nominal: 1, remaining: null });
    });

    it("reports remaining as unknown when even one band (of several) is missing member state", () => {
        const bands = [
            band(0, [member("sda1", false), member("sdb1", false)]),
            band(1, undefined),
        ];
        assert.deepEqual(groupToleranceStatus("shr2", bands), { nominal: 2, remaining: null });
    });

    it("an unrecognized mode leaves both nominal and remaining null", () => {
        assert.deepEqual(
            groupToleranceStatus("mystery-mode", [band(0, [member("sda1", false)])]),
            { nominal: null, remaining: null },
        );
    });

    it("a group with no bands at all reports remaining as unknown, not healthy", () => {
        assert.deepEqual(groupToleranceStatus("shr", []), { nominal: 1, remaining: null });
    });
});

describe("formatDuration", () => {
    it("renders hours+minutes, sub-minute, and unknown durations", () => {
        assert.equal(formatDuration(125), "2 h 5 min");
        assert.equal(formatDuration(45), "45 min");
        assert.equal(formatDuration(0.4), "less than a minute");
        assert.equal(formatDuration(null), "Unknown");
    });
});

describe("formatSyncProgress", () => {
    it("renders idle, in-progress-with-unknown-percent, and full progress", () => {
        assert.equal(formatSyncProgress(null), "Idle (nothing in progress)");
        assert.equal(
            formatSyncProgress({ action: "recovery", percent: null, finish_min: null }),
            "Recovery · progress unknown · Unknown remaining",
        );
        assert.equal(
            formatSyncProgress({ action: "resync", percent: 42.5, finish_min: 8.2 }),
            "Resync · 42.5% · 8 min remaining",
        );
    });
});

describe("describeSyncAction / describeArrayState", () => {
    it("translates the kernel's own words", () => {
        assert.equal(describeSyncAction("reshape"), "Capacity reshape");
        assert.equal(describeSyncAction("check"), "Integrity check");
        assert.equal(describeArrayState("active"), "Active");
        assert.equal(describeArrayState("inactive"), "Stopped");
    });

    it("passes an unrecognised value through rather than hiding it", () => {
        assert.equal(describeSyncAction("frobnicate"), "frobnicate");
        assert.equal(describeArrayState("read-auto"), "read-auto");
    });
});

// `ArrayRow` (panels.tsx) used to compute its own percent/ETA text
// inline, and the ETA half printed raw minutes ("about 540.0 min") instead of
// routing through `formatDuration` like `formatSyncProgress` did one panel
// below for bands -- the fix pulled the fragment out into this function so
// it has a test surface at all (panels.tsx itself has no render test).
describe("formatSyncPercentEta", () => {
    it("renders idle as an empty string", () => {
        assert.equal(formatSyncPercentEta(null), "");
    });

    it("renders an unknown percent honestly, with no ETA suffix when finish_min is also null", () => {
        assert.equal(
            formatSyncPercentEta({ action: "recovery", percent: null, finish_min: null }),
            "calculating progress",
        );
    });

    it("renders a known percent with no ETA suffix when finish_min is null", () => {
        assert.equal(
            formatSyncPercentEta({ action: "repair", percent: 5, finish_min: null }),
            "5.0%",
        );
    });

    it("renders a sub-hour ETA in minutes only", () => {
        assert.equal(
            formatSyncPercentEta({ action: "resync", percent: 42.5, finish_min: 8.2 }),
            "42.5% · about 8 min",
        );
    });

    // Observed live on a scrubbing guest: "about less than a minute"
    // (Korean "약 1분 미만"). `formatDuration`'s under-a-minute answer is
    // already the approximation, so the "about" prefix reads as a mistake
    // stacked on top of it. Every OTHER duration still keeps the prefix --
    // an ETA of exactly "8 min" IS a rounded estimate and must say so.
    it("drops the \"about\" prefix for an ETA under a minute, which is already approximate", () => {
        assert.equal(
            formatSyncPercentEta({ action: "check", percent: 28.7, finish_min: 0.4 }),
            "28.7% · less than a minute",
        );
    });

    // The task's own reported symptom: a 9-hour rebuild must read "9 h
    // 0 min" here exactly like `formatSyncProgress` does for the same
    // finish_min, never "540.0 min".
    it("renders a multi-hour ETA via formatDuration, not raw minutes", () => {
        assert.equal(
            formatSyncPercentEta({ action: "recovery", percent: 12.3, finish_min: 540 }),
            "12.3% · about 9 h 0 min",
        );
    });
});

describe("formatScrub", () => {
    it("prioritizes in-progress, then flags a nonzero error count as a warning", () => {
        assert.deepEqual(formatScrub(null, true), { text: "Scrub in progress", tone: "neutral" });
        assert.deepEqual(formatScrub(null, false), { text: "No scrub history", tone: "neutral" });
        assert.deepEqual(
            formatScrub({ finished_at: "2026-07-24T10:15:00Z", outcome: "completed", error_count: 0 }, false),
            { text: "2026-07-24T10:15:00Z completed", tone: "good" },
        );
        assert.deepEqual(
            formatScrub({ finished_at: "2026-07-24T09:30:00Z", outcome: "failed", error_count: 3 }, false),
            { text: "2026-07-24T09:30:00Z failed · 3 errors", tone: "warning" },
        );
    });
});

describe("parseFsDfOutput", () => {
    const validFsDf = {
        schema_version: 2,
        groups: [{
            name: "shr1",
            mount_point: "/mnt/shr_data",
            usable_bytes: 6_000_000_000_000,
            data_used_bytes: null,
            data_total_bytes: null,
            metadata_used_bytes: null,
            metadata_total_bytes: null,
            unallocated_bytes: null,
            statvfs_avail_bytes: null,
        }],
    };

    it("accepts the schema v2 fs df contract, preserving honest nulls", () => {
        const parsed = parseFsDfOutput(JSON.stringify(validFsDf));
        assert.deepEqual(parsed, validFsDf);
    });

    it("accepts real (non-null) usage figures now that fs df has a live Btrfs parser", () => {
        const withRealUsage = {
            schema_version: 2,
            groups: [{
                name: "shr1",
                mount_point: "/mnt/shr_data",
                usable_bytes: 6_000_000_000_000,
                data_used_bytes: 3_187_671_040,
                data_total_bytes: 5_368_709_120,
                metadata_used_bytes: 33_554_432,
                metadata_total_bytes: 1_073_741_824,
                unallocated_bytes: 15_032_385_536,
                statvfs_avail_bytes: 17_825_792_000,
            }],
        };
        const parsed = parseFsDfOutput(JSON.stringify(withRealUsage));
        assert.deepEqual(parsed, withRealUsage);
    });

    it("accepts a group with some real figures and some genuinely-missing ones side by side", () => {
        // Btrfs's own output can carry data_used/data_total but not
        // metadata_* (see BtrfsUsage's doc comment in shr-exec) -- a mix of
        // real numbers and honest nulls on the SAME group must round-trip,
        // not be forced to all-or-nothing.
        const mixed = {
            schema_version: 2,
            groups: [{
                name: "shr1",
                mount_point: "/mnt/shr_data",
                usable_bytes: 6_000_000_000_000,
                data_used_bytes: 3_187_671_040,
                data_total_bytes: 5_368_709_120,
                metadata_used_bytes: null,
                metadata_total_bytes: null,
                unallocated_bytes: 15_032_385_536,
                statvfs_avail_bytes: null,
            }],
        };
        assert.deepEqual(parseFsDfOutput(JSON.stringify(mixed)), mixed);
    });

    it("rejects malformed JSON, CLI error objects, and unknown schemas", () => {
        assert.throws(() => parseFsDfOutput("not json"), /valid fs df JSON/);
        assert.throws(
            () => parseFsDfOutput(JSON.stringify({ error: "no state.toml" })),
            /no state\.toml/,
        );
        assert.throws(
            () => parseFsDfOutput(JSON.stringify({ ...validFsDf, schema_version: 3 })),
            /so usage cannot be read/,
        );
    });

    it("rejects a malformed group row instead of rendering garbage", () => {
        assert.throws(
            () => parseFsDfOutput(JSON.stringify({ ...validFsDf, groups: [{}] })),
            /fs df response, group entry #/,
        );
    });
});

// The dashboard's top summary strip must surface active background RAID
// activity (resync/recovery/reshape/check), derived strictly from
// `ArrayStatus.sync` -- the live mdstat-sourced signal -- never from a
// group's `scrub_in_progress`/resize flags in state.toml (rule 2: trust
// kernel state, not recorded state).
describe("classifySyncAction", () => {
    it("maps mdadm's four documented sync_action values, case/whitespace-insensitively", () => {
        assert.equal(classifySyncAction("recovery"), "recovery");
        assert.equal(classifySyncAction("reshape"), "reshape");
        assert.equal(classifySyncAction("resync"), "resync");
        assert.equal(classifySyncAction("check"), "check");
        assert.equal(classifySyncAction("  RESYNC  "), "resync");
    });

    it("falls back to 'other' for a value it doesn't recognize, rather than misclassifying it", () => {
        assert.equal(classifySyncAction("repair"), "other");
        assert.equal(classifySyncAction(""), "other");
    });
});

describe("activeBackgroundSyncs", () => {
    const makeArray = (name: string, sync: ArrayStatus["sync"]): ArrayStatus => ({
        name,
        level: "raid5",
        state: "active",
        read_only: false,
        degraded: false,
        raid_disks: null,
        active_disks: null,
        members: [],
        member_states: [],
        sync,
    });

    it("excludes idle arrays (sync: null) and maps the rest with their classified kind", () => {
        const arrays = [
            makeArray("md0", null),
            makeArray("md1", { action: "recovery", percent: 12.3, finish_min: 40 }),
        ];
        assert.deepEqual(activeBackgroundSyncs(arrays), [
            { arrayName: "md1", action: "recovery", kind: "recovery", percent: 12.3, finishMin: 40 },
        ]);
    });

    it("returns an empty list when every array is idle", () => {
        assert.deepEqual(activeBackgroundSyncs([makeArray("md0", null), makeArray("md1", null)]), []);
    });
});

describe("describeBackgroundActivity", () => {
    const makeArray = (name: string, sync: ArrayStatus["sync"]): ArrayStatus => ({
        name,
        level: "raid5",
        state: "active",
        read_only: false,
        degraded: false,
        raid_disks: null,
        active_disks: null,
        members: [],
        member_states: [],
        sync,
    });

    // Minimal GroupStatus with one band, standing in for a state.toml that
    // expects a live array. Field values otherwise irrelevant to this function.
    const makeGroupWithBand = (): GroupStatus => ({
        name: "vault",
        mode: "shr1",
        layout_version: 1,
        mount_point: "/mnt/shr_data",
        fs_uuid: null,
        usable_bytes: 0,
        resize_pending: false,
        disks: [],
        bands: [{
            index: 0,
            level: "raid5",
            md_name: "md0",
            usable_bytes: 0,
            resize_pending: false,
            members: [],
            sync: null,
            last_scrub: null,
            scrub_in_progress: false,
        }],
    });

    it("reads calm and idle when nothing is running -- no warning tone, no scary empty state", () => {
        assert.deepEqual(
            describeBackgroundActivity([makeArray("md0", null), makeArray("md1", null)], [makeGroupWithBand()]),
            { tone: "good", headline: "Idle", detail: "No background work in progress" },
        );
    });

    it("flags a scrub (check) as routine -- neutral tone, not the same reading as a rebuild", () => {
        const arrays = [makeArray("md0", { action: "check", percent: 57.3, finish_min: 12.5 })];
        assert.deepEqual(describeBackgroundActivity(arrays, [makeGroupWithBand()]), {
            tone: "neutral",
            headline: "Scrub in progress",
            // FormatDuration rounds to whole minutes (round-half-up), same as
            // formatSyncProgress already did for this exact field elsewhere -- 12.5
            // is not a special case that keeps its fraction.
            detail: "md0 Scrub · 57.3% · about 13 min",
        });
    });

    it("flags a recovery (rebuild) as warning-tier -- reduced/lost redundancy while it runs", () => {
        const arrays = [makeArray("md0", { action: "recovery", percent: null, finish_min: null })];
        assert.deepEqual(describeBackgroundActivity(arrays, [makeGroupWithBand()]), {
            tone: "warning",
            headline: "Recovery in progress",
            // Never fabricate a percent/ETA mdadm didn't report -- see ArrayRow's
            // identical wording in panels.tsx for the established honesty phrase.
            detail: "md0 Recovery · calculating progress",
        });
    });

    it("flags a reshape (expansion) as warning-tier like recovery, distinctly labeled", () => {
        const arrays = [makeArray("md0", { action: "reshape", percent: 8, finish_min: 300 })];
        assert.deepEqual(describeBackgroundActivity(arrays, [makeGroupWithBand()]), {
            tone: "warning",
            headline: "Expansion in progress",
            // This ETA must render via formatDuration ("5 h 0 min"), the same
            // helper formatSyncProgress already uses -- not an inline `.toFixed(1)} min`
            // that reads "300.0 min" one panel above the correctly-formatted duration.
            detail: "md0 Expansion · 8.0% · about 5 h 0 min",
        });
    });

    // The task's own reported symptom -- a 9-hour rebuild ETA rendered as
    // "about 540.0 min" here (and in ArrayRow) while formatSyncProgress, one panel
    // below, correctly said "9 h 0 min" for the identical minute count.
    it("formats a multi-hour ETA the same way formatSyncProgress does, not as raw minutes", () => {
        const arrays = [makeArray("md0", { action: "recovery", percent: 12.3, finish_min: 540 })];
        assert.deepEqual(describeBackgroundActivity(arrays, [makeGroupWithBand()]), {
            tone: "warning",
            headline: "Recovery in progress",
            detail: "md0 Recovery · 12.3% · about 9 h 0 min",
        });
    });

    it("passes through an action mdadm reports that isn't one of the four documented kinds", () => {
        const arrays = [makeArray("md0", { action: "repair", percent: 5, finish_min: null })];
        assert.deepEqual(describeBackgroundActivity(arrays, [makeGroupWithBand()]), {
            tone: "neutral",
            headline: "repair in progress",
            detail: "md0 repair · 5.0%",
        });
    });

    it("with several arrays busy at once, leads with the worst kind and lists every array individually -- never an averaged percent", () => {
        const arrays = [
            makeArray("md0", { action: "recovery", percent: null, finish_min: null }),
            makeArray("md1", { action: "check", percent: 57.3, finish_min: 12.5 }),
        ];
        assert.deepEqual(describeBackgroundActivity(arrays, [makeGroupWithBand()]), {
            tone: "warning",
            headline: "Recovery and 1 other in progress",
            // Same formatDuration rounding as the scrub test above.
            detail: "md0 Recovery · calculating progress / md1 Scrub · 57.3% · about 13 min",
        });
    });

    // Measured live on a real guest with the array stopped (umount +
    // vgchange -an + mdadm --stop) but state.toml still describing a band --
    // `shr-rs status --json` then reports `health: degraded`, `arrays: []`.
    // The old code read `arrays === []` as idle/green, the same defect
    // an earlier fix already addressed one card below on BandRow's sync cell (panels.tsx).
    it("flags an empty array list as unable-to-verify, not idle/green, when a group expects bands", () => {
        assert.deepEqual(describeBackgroundActivity([], [makeGroupWithBand()]), {
            tone: "warning",
            headline: "Cannot verify",
            detail: "No live array information",
        });
    });

    // Guard against over-correcting back into a naive `arrays.length ===
    // 0` check: a fresh host with no groups configured at all also has
    // `arrays === []`, and for that host idle/green is correct -- there is
    // genuinely nothing to verify, unlike the case above where a band was
    // expected and didn't answer.
    it("still reads calm and idle when no groups are configured at all, even with an empty array list", () => {
        assert.deepEqual(describeBackgroundActivity([], []), {
            tone: "good",
            headline: "Idle",
            detail: "No background work in progress",
        });
    });
});
