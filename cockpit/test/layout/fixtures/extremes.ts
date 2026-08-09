/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * The same page pushed to its boundaries: the longest identifiers a real
 * `/dev/disk/by-id` name reaches, every optional field absent, a drive whose
 * size and SMART state are unknown, two groups so the operations panel repeats,
 * a rebuild in flight, and no `fs df` answer at all.
 *
 * These are the values that break a layout, and they are the reason the audit
 * runs each viewport twice. A 4 TB nominal string fits everywhere; the 47
 * character by-id name below is what pushes a table cell past its column and
 * what a Korean label has to sit beside without either being clipped.
 */

import type { FsDfReport, StatusReport } from "../../../src/model.ts";
import type { HarnessFixture } from "../harness/cockpitStub.ts";

const TB = 1_000_000_000_000;

const unknownSmart = {
    state: "unknown" as const,
    temperature_c: null,
    power_on_hours: null,
    pending_sectors: null,
    reallocated_sectors: null,
    uncorrectable_sectors: null,
    nvme_critical_warning: null,
};

const status: StatusReport = {
    schema_version: 2,
    health: "degraded",
    disks: [
        {
            name: "nvme0n1",
            id: "nvme-Samsung_SSD_990_PRO_with_Heatsink_4TB_S7KGNU0X512345B_1",
            size: 4 * TB,
            model: "Samsung SSD 990 PRO with Heatsink 4TB",
            serial: "S7KGNU0X512345B",
            rotational: false,
            smart: {
                state: "warning",
                temperature_c: 71,
                power_on_hours: 41_233,
                pending_sectors: 0,
                reallocated_sectors: 0,
                uncorrectable_sectors: 0,
                nvme_critical_warning: 4,
            },
            arrays: [],
            system_disk: true,
            system_mounts: ["/", "/boot", "/boot/efi", "/home", "/var", "/var/log"],
        },
        {
            name: "sda",
            id: "ata-TOSHIBA_MG09ACA18TE_ABCDEFGHIJKLMNOPQRSTUVWX",
            size: 18 * TB,
            model: "TOSHIBA MG09ACA18TE Enterprise Capacity Hard Drive",
            serial: "ABCDEFGHIJKLMNOPQRSTUVWX",
            rotational: true,
            smart: {
                state: "warning",
                temperature_c: 58,
                power_on_hours: 63_912,
                pending_sectors: 184,
                reallocated_sectors: 96,
                uncorrectable_sectors: 12,
                nvme_critical_warning: null,
            },
            arrays: ["md0", "md1", "md2"],
            system_disk: false,
            system_mounts: [],
        },
        {
            name: "sdb",
            id: "ata-TOSHIBA_MG09ACA18TE_ABCDEFGHIJKLMNOPQRSTUVWY",
            size: 18 * TB,
            model: "TOSHIBA MG09ACA18TE Enterprise Capacity Hard Drive",
            serial: "ABCDEFGHIJKLMNOPQRSTUVWY",
            rotational: true,
            smart: unknownSmart,
            arrays: ["md0", "md1", "md2"],
            system_disk: false,
            system_mounts: [],
        },
        {
            name: "sdc",
            id: null,
            size: null,
            model: null,
            serial: null,
            rotational: null,
            smart: unknownSmart,
            arrays: [],
        },
        {
            name: "sdd",
            id: "ata-WDC_WD140EDGZ-11B1PA0_9LGXYZ0K",
            size: 14 * TB,
            model: "WDC WD140EDGZ-11B1PA0",
            serial: "9LGXYZ0K",
            rotational: true,
            smart: {
                state: "ok",
                temperature_c: 41,
                power_on_hours: 2,
                pending_sectors: 0,
                reallocated_sectors: 0,
                uncorrectable_sectors: 0,
                nvme_critical_warning: null,
            },
            arrays: ["md2"],
            system_disk: false,
            system_mounts: [],
        },
        {
            name: "sde",
            id: "ata-WDC_WD140EDGZ-11B1PA0_9LGXYZ0L",
            size: 14 * TB,
            model: "WDC WD140EDGZ-11B1PA0",
            serial: "9LGXYZ0L",
            rotational: true,
            smart: unknownSmart,
            arrays: [],
            system_disk: false,
            system_mounts: [],
        },
    ],
    arrays: [
        {
            name: "md0",
            level: "raid1",
            state: "active, degraded, recovering",
            read_only: false,
            degraded: true,
            raid_disks: 2,
            active_disks: 1,
            members: ["sda1", "sdb1"],
            member_states: [
                { name: "sda1", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
                { name: "sdb1", role: null, faulty: true, spare: false, write_mostly: false, replacement: false },
            ],
            sync: { action: "recovery", percent: 99.9, finish_min: 1_874.3 },
        },
        {
            name: "md1",
            level: "raid6",
            state: "active",
            read_only: true,
            degraded: false,
            raid_disks: 4,
            active_disks: 4,
            members: ["sda2", "sdb2", "sdd1", "sde1"],
            member_states: [
                { name: "sda2", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
                { name: "sdb2", role: 1, faulty: false, spare: false, write_mostly: true, replacement: false },
                { name: "sdd1", role: 2, faulty: false, spare: false, write_mostly: false, replacement: true },
                { name: "sde1", role: null, faulty: false, spare: true, write_mostly: false, replacement: false },
            ],
            sync: { action: "reshape", percent: 0.1, finish_min: null },
        },
        {
            name: "md2",
            level: null,
            state: "inactive",
            read_only: false,
            degraded: false,
            raid_disks: null,
            active_disks: null,
            members: [],
            sync: null,
        },
    ],
    groups: [
        {
            name: "photographic-archive-2026",
            mode: "shr",
            layout_version: 2,
            mount_point: "/mnt/photographic-archive-2026",
            fs_uuid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
            usable_bytes: 46 * TB,
            resize_pending: true,
            disks: [
                "ata-TOSHIBA_MG09ACA18TE_ABCDEFGHIJKLMNOPQRSTUVWX",
                "ata-TOSHIBA_MG09ACA18TE_ABCDEFGHIJKLMNOPQRSTUVWY",
                "ata-WDC_WD140EDGZ-11B1PA0_9LGXYZ0K",
                "ata-WDC_WD140EDGZ-11B1PA0_9LGXYZ0L",
            ],
            vg_name: "photographic_archive_2026_vg",
            lv_name: "data",
            compression: "zstd:15",
            bands: [
                {
                    index: 0,
                    level: "raid1",
                    md_name: "md0",
                    usable_bytes: 14 * TB,
                    resize_pending: true,
                    members: ["sda1", "sdb1"],
                    member_states: [
                        { name: "sda1", role: 0, faulty: false, spare: false, write_mostly: false, replacement: false },
                        { name: "sdb1", role: null, faulty: true, spare: false, write_mostly: false, replacement: false },
                    ],
                    md_uuid: "3aa5f0c2:1c9d4e77:6b2f8d10:4e7a9c33",
                    sync: { action: "recovery", percent: 99.9, finish_min: 1_874.3 },
                    last_scrub: { finished_at: "2026-01-01T00:00:00Z", outcome: "failed", error_count: 2_147_483_647 },
                    scrub_in_progress: true,
                },
                {
                    index: 1,
                    level: "raid6",
                    md_name: "md1",
                    usable_bytes: 32 * TB,
                    resize_pending: false,
                    members: ["sda2", "sdb2", "sdd1", "sde1"],
                    md_uuid: null,
                    sync: { action: "reshape", percent: 0.1, finish_min: null },
                    last_scrub: { finished_at: "2025-12-31T23:59:59Z", outcome: "cancelled", error_count: 0 },
                    scrub_in_progress: false,
                },
            ],
        },
        {
            // No bands and no LVM names: a group recorded in state.toml whose
            // arrays have not assembled. Every capacity figure downstream of it
            // is zero, which is a layout case of its own.
            name: "scratch",
            mode: "shr",
            layout_version: 2,
            mount_point: "/mnt/scratch",
            fs_uuid: null,
            usable_bytes: 0,
            resize_pending: false,
            disks: [],
            bands: [],
        },
    ],
    state_path: null,
};

/* `fs df` failing is the normal state on an unmounted group, and the capacity
   panel is meant to degrade rather than disappear. */
const fsDf: FsDfReport | null = null;

export const extremes: HarnessFixture = { status, fsDf };
