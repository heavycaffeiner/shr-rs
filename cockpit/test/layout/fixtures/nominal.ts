/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * The everyday page: one healthy SHR group over four drives, nothing syncing,
 * every optional field filled in. Typed against `model.ts` rather than cast,
 * so a schema change breaks `npm run typecheck` instead of surfacing as a
 * puzzling audit failure.
 */

import type { FsDfReport, StatusReport } from "../../../src/model.ts";
import type { HarnessFixture } from "../harness/cockpitStub.ts";

const TB = 1_000_000_000_000;

const healthySmart = (temperature: number, hours: number) => ({
    state: "ok" as const,
    temperature_c: temperature,
    power_on_hours: hours,
    pending_sectors: 0,
    reallocated_sectors: 0,
    uncorrectable_sectors: 0,
    nvme_critical_warning: null,
});

const status: StatusReport = {
    schema_version: 2,
    health: "healthy",
    disks: [
        {
            name: "sda",
            id: "ata-Samsung_SSD_870_EVO_500GB_S6PWNZ0T101234A",
            size: 500_000_000_000,
            model: "Samsung SSD 870 EVO 500GB",
            serial: "S6PWNZ0T101234A",
            rotational: false,
            smart: healthySmart(31, 9_400),
            arrays: [],
            system_disk: true,
            system_mounts: ["/", "/boot", "/boot/efi"],
        },
        {
            name: "sdb",
            id: "ata-WDC_WD40EFPX-68C6CN0_WD-WX12D34ABCDE",
            size: 4 * TB,
            model: "WDC WD40EFPX-68C6CN0",
            serial: "WD-WX12D34ABCDE",
            rotational: true,
            smart: healthySmart(35, 12_800),
            arrays: ["md0", "md1"],
            system_disk: false,
            system_mounts: [],
        },
        {
            name: "sdc",
            id: "ata-WDC_WD40EFPX-68C6CN0_WD-WX12D34ABCDF",
            size: 4 * TB,
            model: "WDC WD40EFPX-68C6CN0",
            serial: "WD-WX12D34ABCDF",
            rotational: true,
            smart: healthySmart(36, 12_790),
            arrays: ["md0", "md1"],
            system_disk: false,
            system_mounts: [],
        },
        {
            name: "sdd",
            id: "ata-ST8000VN004-3CP101_WSD1A2B3",
            size: 8 * TB,
            model: "ST8000VN004-3CP101",
            serial: "WSD1A2B3",
            rotational: true,
            smart: healthySmart(38, 4_120),
            arrays: ["md1"],
            system_disk: false,
            system_mounts: [],
        },
    ],
    arrays: [
        {
            name: "md0",
            level: "raid1",
            state: "active",
            read_only: false,
            degraded: false,
            raid_disks: 2,
            active_disks: 2,
            members: ["sdb1", "sdc1"],
            member_states: [
                { name: "sdb1", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
                { name: "sdc1", role: 1, faulty: false, spare: false, write_mostly: false, replacement: false },
            ],
            sync: null,
        },
        {
            name: "md1",
            level: "raid5",
            state: "active",
            read_only: false,
            degraded: false,
            raid_disks: 3,
            active_disks: 3,
            members: ["sdb2", "sdc2", "sdd1"],
            member_states: [
                { name: "sdb2", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
                { name: "sdc2", role: 1, faulty: false, spare: false, write_mostly: false, replacement: false },
                { name: "sdd1", role: 2, faulty: false, spare: false, write_mostly: false, replacement: false },
            ],
            sync: null,
        },
    ],
    groups: [
        {
            name: "tank",
            mode: "shr",
            layout_version: 2,
            mount_point: "/mnt/tank",
            fs_uuid: "6f9619ff-8b86-4d01-b42d-00cf4fc964ff",
            usable_bytes: 8 * TB,
            resize_pending: false,
            disks: [
                "ata-WDC_WD40EFPX-68C6CN0_WD-WX12D34ABCDE",
                "ata-WDC_WD40EFPX-68C6CN0_WD-WX12D34ABCDF",
                "ata-ST8000VN004-3CP101_WSD1A2B3",
            ],
            vg_name: "tank_vg",
            lv_name: "data",
            compression: "zstd:3",
            bands: [
                {
                    index: 0,
                    level: "raid1",
                    md_name: "md0",
                    usable_bytes: 4 * TB,
                    resize_pending: false,
                    members: ["sdb1", "sdc1"],
                    member_states: [
                        { name: "sdb1", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
                        { name: "sdc1", role: 1, faulty: false, spare: false, write_mostly: false, replacement: false },
                    ],
                    md_uuid: "3aa5f0c2:1c9d4e77:6b2f8d10:4e7a9c33",
                    sync: null,
                    last_scrub: { finished_at: "2026-07-28T02:14:07Z", outcome: "completed", error_count: 0 },
                    scrub_in_progress: false,
                },
                {
                    index: 1,
                    level: "raid5",
                    md_name: "md1",
                    usable_bytes: 4 * TB,
                    resize_pending: false,
                    members: ["sdb2", "sdc2", "sdd1"],
                    member_states: [
                        { name: "sdb2", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
                        { name: "sdc2", role: 1, faulty: false, spare: false, write_mostly: false, replacement: false },
                        { name: "sdd1", role: 2, faulty: false, spare: false, write_mostly: false, replacement: false },
                    ],
                    md_uuid: "9c4d1e8b:22f60a35:78bd4419:0f5e6a2d",
                    sync: null,
                    last_scrub: { finished_at: "2026-07-28T03:41:52Z", outcome: "completed", error_count: 0 },
                    scrub_in_progress: false,
                },
            ],
        },
    ],
    state_path: "/var/lib/shr-rs/state.toml",
};

const fsDf: FsDfReport = {
    schema_version: 2,
    groups: [
        {
            name: "tank",
            mount_point: "/mnt/tank",
            usable_bytes: 8 * TB,
            data_used_bytes: 3_142_000_000_000,
            data_total_bytes: 3_400_000_000_000,
            metadata_used_bytes: 4_100_000_000,
            metadata_total_bytes: 8_589_934_592,
            unallocated_bytes: 4_591_410_065_408,
            statvfs_avail_bytes: 4_849_410_065_408,
        },
    ],
};

export const nominal: HarnessFixture = { status, fsDf };
