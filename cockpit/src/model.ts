export type Health = "healthy" | "degraded" | "unknown";
export type SmartState = "ok" | "warning" | "unknown";

export interface SmartSummary {
    state: SmartState;
    temperature_c: number | null;
    power_on_hours: number | null;
    pending_sectors: number | null;
    reallocated_sectors: number | null;
    uncorrectable_sectors: number | null;
    nvme_critical_warning: number | null;
}

export interface DiskStatus {
    name: string;
    // Stable /dev/disk/by-id name -- conditionally serialized on the Rust
    // side (`#[serde(skip_serializing_if = "Option::is_none")]`), so it may
    // be absent entirely, not just `null`; see `parseDisk`. Optional here
    // (rather than required) so other files' `DiskStatus` object literals
    // (e.g. actions.test.ts's disk fixture factory, predating that fix) don't
    // have to be updated just to keep typechecking -- `parseDisk` itself
    // always fills all three fields in, never leaves them `undefined`.
    id?: string | null;
    size: number | null;
    model: string | null;
    serial: string | null;
    rotational: boolean | null;
    smart: SmartSummary;
    arrays: string[];
    // True when this disk holds OS mounts (/, /boot, ...) -- `#[serde(default)]`
    // on the Rust side, so it may be entirely absent on an older payload.
    system_disk?: boolean;
    // Observed system mountpoints under this disk; conditionally serialized
    // (empty is omitted), so also may be entirely absent.
    system_mounts?: string[];
}

export interface SyncSummary {
    action: string;
    percent: number | null;
    finish_min: number | null;
}

export type ScrubOutcome = "completed" | "cancelled" | "failed";

export interface ScrubSummary {
    finished_at: string;
    outcome: ScrubOutcome;
    error_count: number;
}

// Per-member mdstat detail. `members: string[]` (on both `ArrayStatus`
// and `GroupBandStatus`) stays the raw live inventory, faulty devices
// included -- `member_states` is the parallel array that says which of those
// names are actually healthy right now. See `annotateMembers` for how the
// two are combined for display. Health only -- never feeds capacity/parity
// math, which uses `ArrayStatus.raid_disks` instead (see `computeBandCapacity`).
export interface MemberStatus {
    name: string;
    role: number | null;
    faulty: boolean;
    spare: boolean;
    write_mostly: boolean;
    replacement: boolean;
}

export interface ArrayStatus {
    name: string;
    level: string | null;
    state: string;
    read_only: boolean;
    degraded: boolean;
    raid_disks: number | null;
    active_disks: number | null;
    members: string[];
    // Rust always serializes this once `array_status()` runs (never
    // conditionally omitted), but kept optional here -- same reasoning as
    // `DiskStatus.id` below -- so older fixtures elsewhere don't need
    // updating just to keep typechecking. `parseArray` fills it in with `[]`
    // when genuinely absent, matching the "fall back to today's behavior"
    // rule every consumer of this field follows.
    member_states?: MemberStatus[];
    sync: SyncSummary | null;
}

export interface GroupBandStatus {
    index: number;
    level: string;
    md_name: string;
    usable_bytes: number;
    resize_pending: boolean;
    // Live mdstat-sourced fields (added this wave) -- may be absent on an
    // older-but-still-schema-v2 payload, see `parseGroupBand`'s defaults.
    members: string[];
    // Additive (earlier wave). `#[serde(skip_serializing_if = "Vec::is_empty")]`
    // on the Rust side -- omitted entirely when the band isn't live, not
    // just an empty array. Optional here for the same reason as `id` below.
    member_states?: MemberStatus[];
    // `#[serde(skip_serializing_if = "Option::is_none")]` -- omitted when
    // the UUID isn't known yet (band never assembled). Optional here for the
    // same reason as `id` below.
    md_uuid?: string | null;
    sync: SyncSummary | null;
    last_scrub: ScrubSummary | null;
    scrub_in_progress: boolean;
}

export interface GroupStatus {
    name: string;
    mode: string;
    layout_version: number;
    mount_point: string;
    fs_uuid: string | null;
    usable_bytes: number;
    resize_pending: boolean;
    disks: string[];
    bands: GroupBandStatus[];
    // Plain (non-`Option`) `String`s on the Rust side, always present once a
    // group exists -- `parseGroup` requires them and throws if a real
    // payload is missing one. Optional here (rather than required) purely so
    // other files' `GroupStatus` object literals predating this field (e.g.
    // actions.test.ts's group fixture factory) don't have to be updated just
    // to keep typechecking -- see `DiskStatus.id`'s doc comment for the same
    // pattern.
    vg_name?: string;
    lv_name?: string;
    compression?: string;
}

export interface StatusReport {
    schema_version: 2;
    health: Health;
    disks: DiskStatus[];
    arrays: ArrayStatus[];
    groups: GroupStatus[];
    // The state.toml path this invocation resolved. null when the
    // backend omits it (older CLI build, or a caller that never learned the
    // path -- see StatusReport::state_path's doc comment in
    // crates/shr-command/src/report.rs). Never fabricated in panels.tsx.
    state_path: string | null;
}

export interface StatusSummary {
    rawBytes: number | null;
    unknownSizeDisks: number;
    linkedDisks: number;
    unlinkedDisks: number;
    /** Disks holding OS mounts (`disk.system_disk`) -- see `unlinkedDisks`'s doc comment. */
    systemDisks: number;
    warningDisks: number;
    warningArrays: number;
    activeMembers: number;
    expectedMembers: number;
}

// --- `shr-rs fs df --json` (FsDfReport) -------------------------------------
// `build_fs_df` is now backed by a live `btrfs filesystem usage --raw` / `df`
// parser, so these fields carry real numbers on a
// mounted, working group. Any individual field can still read `null` -- an
// unmounted group, a `btrfs`/`df` invocation failure, or a figure genuinely
// absent from Btrfs's own output -- and that null is never backfilled with a
// guess (see `fs_usage_input`'s `.unwrap_or_default()` behavior).

export interface GroupDfStatus {
    name: string;
    mount_point: string;
    usable_bytes: number;
    data_used_bytes: number | null;
    data_total_bytes: number | null;
    metadata_used_bytes: number | null;
    metadata_total_bytes: number | null;
    unallocated_bytes: number | null;
    statvfs_avail_bytes: number | null;
}

export interface FsDfReport {
    schema_version: 2;
    groups: GroupDfStatus[];
}

type JsonRecord = Record<string, unknown>;

// Exported so `createGroup.ts` can parse the OTHER JSON shapes shr-rs emits
// (`preflight --json`, `create --dry-run --json`) with the exact same
// validation rules and error style as this file's own parsers, instead of
// duplicating a second copy of "is this a record / non-negative number /
// string array" -- see that file's own parsers for why: the group-creation
// wizard must trust the backend's shape verbatim, not re-derive it.
export const isRecord = (value: unknown): value is JsonRecord => (
    typeof value === "object" && value !== null && !Array.isArray(value)
);

export const requireRecord = (value: unknown, message: string): JsonRecord => {
    if (!isRecord(value))
        throw new Error(message);
    return value;
};

export const requireString = (value: unknown, message: string): string => {
    if (typeof value !== "string")
        throw new Error(message);
    return value;
};

export const requireBoolean = (value: unknown, message: string): boolean => {
    if (typeof value !== "boolean")
        throw new Error(message);
    return value;
};

export const requireNullableString = (value: unknown, message: string): string | null => {
    if (value === null)
        return null;
    return requireString(value, message);
};

export const requireNullableNumber = (value: unknown, message: string): number | null => {
    if (value === null)
        return null;
    if (typeof value !== "number" || !Number.isFinite(value) || value < 0)
        throw new Error(message);
    return value;
};

export const requireNumber = (value: unknown, message: string): number => {
    if (typeof value !== "number" || !Number.isFinite(value) || value < 0)
        throw new Error(message);
    return value;
};

export const requireStringArray = (value: unknown, message: string): string[] => {
    if (!Array.isArray(value) || value.some(item => typeof item !== "string"))
        throw new Error(message);
    return value;
};

const parseSmart = (value: unknown, diskName: string): SmartSummary => {
    const smart = requireRecord(value, `디스크 정보(${diskName})의 SMART 정보가 올바르지 않습니다.`);
    if (smart.state !== "ok" && smart.state !== "warning" && smart.state !== "unknown")
        throw new Error(`디스크 정보(${diskName})의 SMART 상태가 올바르지 않습니다.`);

    return {
        state: smart.state,
        temperature_c: requireNullableNumber(smart.temperature_c, `디스크 정보(${diskName})의 온도가 올바르지 않습니다.`),
        power_on_hours: requireNullableNumber(smart.power_on_hours, `디스크 정보(${diskName})의 가동 시간이 올바르지 않습니다.`),
        pending_sectors: requireNullableNumber(smart.pending_sectors, `디스크 정보(${diskName})의 보류 섹터 수가 올바르지 않습니다.`),
        reallocated_sectors: requireNullableNumber(smart.reallocated_sectors, `디스크 정보(${diskName})의 재할당 섹터 수가 올바르지 않습니다.`),
        uncorrectable_sectors: requireNullableNumber(smart.uncorrectable_sectors, `디스크 정보(${diskName})의 복구 불가 섹터 수가 올바르지 않습니다.`),
        nvme_critical_warning: requireNullableNumber(smart.nvme_critical_warning, `디스크 정보(${diskName})의 NVMe 경고 값이 올바르지 않습니다.`),
    };
};

const parseDisk = (value: unknown, index: number): DiskStatus => {
    const disk = requireRecord(value, `디스크 정보 #${index + 1}가 올바르지 않습니다.`);
    const name = requireString(disk.name, `디스크 정보 #${index + 1}의 이름이 올바르지 않습니다.`);
    const rotational = disk.rotational;
    if (rotational !== null && typeof rotational !== "boolean")
        throw new Error(`디스크 정보(${name})의 회전식 여부가 올바르지 않습니다.`);

    return {
        name,
        // `id` is conditionally serialized (skip_serializing_if) -- absent
        // entirely and explicit `null` both mean "no stable id known", but a
        // present non-string value is still a malformed payload.
        id: disk.id === undefined
            ? null
            : requireNullableString(disk.id, `디스크 정보(${name})의 by-id 이름이 올바르지 않습니다.`),
        size: requireNullableNumber(disk.size, `디스크 정보(${name})의 용량이 올바르지 않습니다.`),
        model: requireNullableString(disk.model, `디스크 정보(${name})의 모델명이 올바르지 않습니다.`),
        serial: requireNullableString(disk.serial, `디스크 정보(${name})의 일련번호가 올바르지 않습니다.`),
        rotational,
        smart: parseSmart(disk.smart, name),
        arrays: requireStringArray(disk.arrays, `디스크 정보(${name})의 어레이 목록이 올바르지 않습니다.`),
        // `#[serde(default)]` on the Rust side -- absent entirely on an
        // older payload means "not known to be a system disk", not "known
        // to be a data disk", but there's no third state to represent here,
        // so this defaults to `false` like the Rust struct itself does.
        system_disk: disk.system_disk === undefined
            ? false
            : requireBoolean(disk.system_disk, `디스크 정보(${name})의 시스템 디스크 여부가 올바르지 않습니다.`),
        system_mounts: disk.system_mounts === undefined
            ? []
            : requireStringArray(disk.system_mounts, `디스크 정보(${name})의 시스템 마운트 목록이 올바르지 않습니다.`),
    };
};

// `subject` is a caller-supplied, already-localized label naming what is
// being parsed (an array, or a band within a group), so this one parser
// serves both array-level and band-level `sync` without hardcoding either
// noun into a caller that is describing the other.
const parseSync = (value: unknown, subject: string): SyncSummary | null => {
    if (value === null)
        return null;
    const sync = requireRecord(value, `${subject}의 동기화 정보가 올바르지 않습니다.`);
    return {
        action: requireString(sync.action, `${subject}의 동기화 작업명이 올바르지 않습니다.`),
        percent: requireNullableNumber(sync.percent, `${subject}의 동기화 진행률이 올바르지 않습니다.`),
        finish_min: requireNullableNumber(sync.finish_min, `${subject}의 예상 시간이 올바르지 않습니다.`),
    };
};

const parseScrubSummary = (value: unknown, subject: string): ScrubSummary => {
    const scrub = requireRecord(value, `${subject}의 스크럽 정보가 올바르지 않습니다.`);
    if (scrub.outcome !== "completed" && scrub.outcome !== "cancelled" && scrub.outcome !== "failed")
        throw new Error(`${subject}의 스크럽 결과가 올바르지 않습니다.`);
    return {
        finished_at: requireString(scrub.finished_at, `${subject}의 스크럽 완료 시각이 올바르지 않습니다.`),
        outcome: scrub.outcome,
        error_count: requireNumber(scrub.error_count, `${subject}의 스크럽 오류 수가 올바르지 않습니다.`),
    };
};

const parseMemberState = (value: unknown, subject: string, index: number): MemberStatus => {
    const member = requireRecord(value, `${subject}의 멤버 상태 #${index + 1}가 올바르지 않습니다.`);
    return {
        name: requireString(member.name, `${subject}의 멤버 상태 #${index + 1} 이름이 올바르지 않습니다.`),
        role: requireNullableNumber(member.role, `${subject}의 멤버 상태 #${index + 1}의 role이 올바르지 않습니다.`),
        faulty: requireBoolean(member.faulty, `${subject}의 멤버 상태 #${index + 1}의 faulty 값이 올바르지 않습니다.`),
        spare: requireBoolean(member.spare, `${subject}의 멤버 상태 #${index + 1}의 spare 값이 올바르지 않습니다.`),
        write_mostly: requireBoolean(
            member.write_mostly, `${subject}의 멤버 상태 #${index + 1}의 write_mostly 값이 올바르지 않습니다.`,
        ),
        replacement: requireBoolean(
            member.replacement, `${subject}의 멤버 상태 #${index + 1}의 replacement 값이 올바르지 않습니다.`,
        ),
    };
};

// Absent entirely means "not sent" (undefined), which both `ArrayStatus` and
// `GroupBandStatus` default to `[]` for. Only a present-but-malformed value throws.
const parseMemberStates = (value: unknown, subject: string): MemberStatus[] => {
    if (value === undefined)
        return [];
    if (!Array.isArray(value))
        throw new Error(`${subject}의 멤버 상태 목록이 올바르지 않습니다.`);
    return value.map((item, i) => parseMemberState(item, subject, i));
};

const parseArray = (value: unknown, index: number): ArrayStatus => {
    const array = requireRecord(value, `어레이 정보 #${index + 1}가 올바르지 않습니다.`);
    const name = requireString(array.name, `어레이 정보 #${index + 1}의 이름이 올바르지 않습니다.`);

    return {
        name,
        level: requireNullableString(array.level, `어레이 정보(${name})의 RAID 레벨이 올바르지 않습니다.`),
        state: requireString(array.state, `어레이 정보(${name})의 상태가 올바르지 않습니다.`),
        read_only: requireBoolean(array.read_only, `어레이 정보(${name})의 읽기 전용 상태가 올바르지 않습니다.`),
        degraded: requireBoolean(array.degraded, `어레이 정보(${name})의 degraded 상태가 올바르지 않습니다.`),
        raid_disks: requireNullableNumber(array.raid_disks, `어레이 정보(${name})의 목표 멤버 수가 올바르지 않습니다.`),
        active_disks: requireNullableNumber(array.active_disks, `어레이 정보(${name})의 활성 멤버 수가 올바르지 않습니다.`),
        members: requireStringArray(array.members, `어레이 정보(${name})의 멤버 목록이 올바르지 않습니다.`),
        member_states: parseMemberStates(array.member_states, `어레이 정보(${name})`),
        sync: parseSync(array.sync, `어레이 정보(${name})`),
    };
};

const parseGroupBand = (value: unknown, groupName: string, index: number): GroupBandStatus => {
    const band = requireRecord(value, `그룹(${groupName})의 밴드 정보 #${index + 1}가 올바르지 않습니다.`);
    const subject = `그룹(${groupName})의 밴드 #${index + 1}`;
    return {
        index: requireNumber(band.index, `${subject} 인덱스가 올바르지 않습니다.`),
        level: requireString(band.level, `${subject} RAID 레벨이 올바르지 않습니다.`),
        md_name: requireString(band.md_name, `${subject} mdadm 이름이 올바르지 않습니다.`),
        usable_bytes: requireNumber(band.usable_bytes, `${subject} 가용 용량이 올바르지 않습니다.`),
        resize_pending: requireBoolean(band.resize_pending, `${subject} 확장 마무리 대기 상태가 올바르지 않습니다.`),
        // Additive (this wave): absent entirely on an older-but-still-v2
        // payload must default, not throw -- a partial rollout of the
        // CLI/cockpit pair must never blank the whole dashboard
        // (backward-compatibility requirement in the phase brief).
        members: band.members === undefined
            ? []
            : requireStringArray(band.members, `${subject} 참여 멤버 목록이 올바르지 않습니다.`),
        member_states: parseMemberStates(band.member_states, subject),
        md_uuid: (band.md_uuid === undefined || band.md_uuid === null)
            ? null
            : requireString(band.md_uuid, `${subject} mdadm UUID가 올바르지 않습니다.`),
        sync: band.sync === undefined ? null : parseSync(band.sync, subject),
        last_scrub: (band.last_scrub === undefined || band.last_scrub === null)
            ? null
            : parseScrubSummary(band.last_scrub, subject),
        scrub_in_progress: band.scrub_in_progress === undefined
            ? false
            : requireBoolean(band.scrub_in_progress, `${subject} 스크럽 진행 상태가 올바르지 않습니다.`),
    };
};

const parseGroup = (value: unknown, index: number): GroupStatus => {
    const group = requireRecord(value, `그룹 정보 #${index + 1}가 올바르지 않습니다.`);
    const name = requireString(group.name, `그룹 정보 #${index + 1}의 이름이 올바르지 않습니다.`);

    if (!Array.isArray(group.bands))
        throw new Error(`그룹(${name})의 밴드 목록이 올바르지 않습니다.`);

    return {
        name,
        mode: requireString(group.mode, `그룹(${name})의 모드가 올바르지 않습니다.`),
        layout_version: requireNumber(group.layout_version, `그룹(${name})의 레이아웃 버전이 올바르지 않습니다.`),
        mount_point: requireString(group.mount_point, `그룹(${name})의 마운트 지점이 올바르지 않습니다.`),
        fs_uuid: requireNullableString(group.fs_uuid, `그룹(${name})의 파일시스템 UUID가 올바르지 않습니다.`),
        usable_bytes: requireNumber(group.usable_bytes, `그룹(${name})의 가용 용량이 올바르지 않습니다.`),
        resize_pending: requireBoolean(group.resize_pending, `그룹(${name})의 확장 마무리 대기 상태가 올바르지 않습니다.`),
        disks: requireStringArray(group.disks, `그룹(${name})의 멤버 디스크 목록이 올바르지 않습니다.`),
        bands: group.bands.map((band, i) => parseGroupBand(band, name, i)),
        vg_name: requireString(group.vg_name, `그룹(${name})의 볼륨 그룹 이름이 올바르지 않습니다.`),
        lv_name: requireString(group.lv_name, `그룹(${name})의 논리 볼륨 이름이 올바르지 않습니다.`),
        compression: requireString(group.compression, `그룹(${name})의 압축 설정이 올바르지 않습니다.`),
    };
};

export const parseStatusOutput = (raw: string): StatusReport => {
    let value: unknown;
    try {
        value = JSON.parse(raw);
    } catch {
        throw new Error("shr-rs가 유효한 JSON 상태를 반환하지 않았습니다.");
    }

    const report = requireRecord(value, "shr-rs 상태 응답이 객체가 아닙니다.");
    if (typeof report.error === "string")
        throw new Error(report.error);
    // v1 (no `groups`) is rejected outright rather than treated as "groups
    // happens to be empty" -- see SCHEMA_VERSION's doc comment in
    // crates/shr-command/src/report.rs for why those two cases must not be
    // confused. A genuinely old shr-rs binary must fail loudly here, not
    // render a dashboard that silently omits every SHR group.
    if (report.schema_version !== 2)
        throw new Error(
            "이 서버의 shr-rs 버전이 대시보드와 호환되지 않습니다. shr-rs와 Cockpit 플러그인을 같은 버전으로 " +
            `맞춰 주세요. (응답 형식 ${String(report.schema_version)})`,
        );
    if (report.health !== "healthy" && report.health !== "degraded" && report.health !== "unknown")
        throw new Error("상태 응답의 전체 건강 상태가 올바르지 않습니다.");
    if (!Array.isArray(report.disks))
        throw new Error("상태 응답의 디스크 정보가 올바르지 않습니다.");
    if (!Array.isArray(report.arrays))
        throw new Error("상태 응답의 어레이 정보가 올바르지 않습니다.");
    if (!Array.isArray(report.groups))
        throw new Error("상태 응답의 그룹 정보가 올바르지 않습니다.");

    return {
        schema_version: 2,
        health: report.health,
        disks: report.disks.map(parseDisk),
        arrays: report.arrays.map(parseArray),
        groups: report.groups.map(parseGroup),
        // Conditionally serialized on the Rust side (skip_serializing_if),
        // so absent-vs-null both mean "unknown" -- same convention as
        // GroupBandStatus.md_uuid. Never fabricated: a present non-string
        // value is still a malformed payload and must throw, not be coerced.
        state_path: (report.state_path === undefined || report.state_path === null)
            ? null
            : requireString(report.state_path, "상태 응답의 구성 파일 경로가 올바르지 않습니다."),
    };
};

const parseGroupDf = (value: unknown, index: number): GroupDfStatus => {
    const group = requireRecord(value, `fs df 응답의 그룹 정보 #${index + 1}가 올바르지 않습니다.`);
    const name = requireString(group.name, `fs df 응답의 그룹 정보 #${index + 1} 이름이 올바르지 않습니다.`);
    const subject = `그룹(${name})의 fs df`;
    return {
        name,
        mount_point: requireString(group.mount_point, `${subject} 마운트 지점이 올바르지 않습니다.`),
        usable_bytes: requireNumber(group.usable_bytes, `${subject} 가용 용량이 올바르지 않습니다.`),
        data_used_bytes: requireNullableNumber(group.data_used_bytes, `${subject} 데이터 사용량이 올바르지 않습니다.`),
        data_total_bytes: requireNullableNumber(group.data_total_bytes, `${subject} 데이터 총량이 올바르지 않습니다.`),
        metadata_used_bytes: requireNullableNumber(
            group.metadata_used_bytes, `${subject} 메타데이터 사용량이 올바르지 않습니다.`,
        ),
        metadata_total_bytes: requireNullableNumber(
            group.metadata_total_bytes, `${subject} 메타데이터 총량이 올바르지 않습니다.`,
        ),
        unallocated_bytes: requireNullableNumber(group.unallocated_bytes, `${subject} 미할당 용량이 올바르지 않습니다.`),
        statvfs_avail_bytes: requireNullableNumber(
            group.statvfs_avail_bytes, `${subject} df 가용 용량이 올바르지 않습니다.`,
        ),
    };
};

/** Parse `shr-rs fs df --json`'s stdout (`FsDfReport`). */
export const parseFsDfOutput = (raw: string): FsDfReport => {
    let value: unknown;
    try {
        value = JSON.parse(raw);
    } catch {
        throw new Error("shr-rs가 유효한 fs df JSON을 반환하지 않았습니다.");
    }
    const report = requireRecord(value, "fs df 응답이 객체가 아닙니다.");
    if (typeof report.error === "string")
        throw new Error(report.error);
    if (report.schema_version !== 2)
        throw new Error(
            "이 서버의 shr-rs 버전이 대시보드와 호환되지 않아 사용량을 읽을 수 없습니다. shr-rs와 Cockpit " +
            `플러그인을 같은 버전으로 맞춰 주세요. (응답 형식 ${String(report.schema_version)})`,
        );
    if (!Array.isArray(report.groups))
        throw new Error("fs df 응답의 그룹 정보가 올바르지 않습니다.");
    return {
        schema_version: 2,
        groups: report.groups.map(parseGroupDf),
    };
};

export const arrayNeedsAttention = (array: ArrayStatus): boolean => {
    const state = array.state.trim().toLowerCase();
    const invalidRaid6 = array.level?.trim().toLowerCase() === "raid6" &&
        (array.raid_disks ?? array.members.length) < 4;
    return array.degraded ||
        array.read_only ||
        (state !== "active" && state !== "clean") ||
        invalidRaid6;
};

// `null` if ANY size in the list is unknown -- a partial sum would understate
// physical capacity and look like a real (small) number instead of "we don't
// fully know this yet". Shared by `summarizeStatus` and `summarizeAllocation`.
const totalKnownBytes = (sizes: Array<number | null>): number | null => (
    sizes.some(size => size === null) ? null : sizes.reduce((sum: number, size) => sum + (size ?? 0), 0)
);

export const summarizeStatus = (report: StatusReport): StatusSummary => {
    const unknownSizeDisks = report.disks.filter(disk => disk.size === null).length;
    const rawBytes = totalKnownBytes(report.disks.map(disk => disk.size));

    return {
        rawBytes,
        unknownSizeDisks,
        linkedDisks: report.disks.filter(disk => disk.arrays.length > 0).length,
        // A system disk holds no RAID array by design but is not "spare"
        // capacity either -- shr-rs preflight refuses it outright, so it must
        // not inflate the "not attached to any RAID" count an operator reads
        // as addable. Reported separately via `systemDisks` instead.
        unlinkedDisks: report.disks.filter(disk => disk.arrays.length === 0 && !disk.system_disk).length,
        systemDisks: report.disks.filter(disk => disk.system_disk).length,
        warningDisks: report.disks.filter(disk => disk.smart.state === "warning").length,
        warningArrays: report.arrays.filter(arrayNeedsAttention).length,
        activeMembers: report.arrays.reduce(
            (sum, array) => sum + (array.active_disks ?? array.members.length),
            0,
        ),
        expectedMembers: report.arrays.reduce(
            (sum, array) => sum + (array.raid_disks ?? array.members.length),
            0,
        ),
    };
};

export const formatBytes = (bytes: number | null | undefined): string => {
    if (bytes === null || bytes === undefined)
        return "알 수 없음";
    if (bytes === 0)
        return "0 B";

    const units: Array<[string, number]> = [
        ["PB", 1e15],
        ["TB", 1e12],
        ["GB", 1e9],
        ["MB", 1e6],
        ["KB", 1e3],
    ];
    const unit = units.find(([, factor]) => bytes >= factor);
    return unit ? `${(bytes / unit[1]).toFixed(1)} ${unit[0]}` : `${bytes} B`;
};

// --- Band physical/parity capacity -----------------------------------------
// Mirrors `RaidLevel::data_members` (crates/shr-core/src/raid.rs) exactly --
// shr-rs only ever creates raid1/5/6 bands (see that enum's doc comment), so
// this is a closed set, not a guess at an unknown redundancy scheme. `status
// --json` has no raw/parity byte field at all; this derives it client-side
// from `usable_bytes` (known) and the array's CONFIGURED disk count
// (`ArrayStatus.raid_disks`, see `computeBandCapacity`'s doc comment for why
// it must be this and not a live/healthy member count) -- see the phase
// report for why this counts as "derived", not "invented": the arithmetic is
// exactly what created the band.
export const raidDataMembers = (level: string, memberCount: number): number | null => {
    switch (level.trim().toLowerCase()) {
    case "raid1":
        return memberCount > 0 ? 1 : null;
    case "raid5": {
        const data = memberCount - 1;
        return data > 0 ? data : null;
    }
    case "raid6": {
        const data = memberCount - 2;
        return data > 0 ? data : null;
    }
    default:
        return null;
    }
};

export interface BandCapacity {
    /** The array's configured `raid_disks` -- NOT a live/healthy member count. See doc comment below. */
    memberCount: number;
    dataMembers: number;
    memberBytes: number;
    parityBytes: number;
    rawBytes: number;
}

// Geometry -- the per-disk slice size
// and the band's total physical footprint -- must come from the array's
// CONFIGURED disk count (`ArrayStatus.raid_disks`), never from a live/healthy
// member count. A disk failing does not shrink or grow the array's geometry:
// the partition is still there, mdadm still expects a slot for it, a
// degraded RAID5 still occupies exactly as many disk-slots as it always did.
// What changes when a member goes faulty is redundancy margin, not geometry.
// Real-guest measurement that caught the original fix getting this
// wrong: raid_disks=3, usable_bytes=8.6 GB, one of 3 live members faulty --
// correct slice/total is 4.3 GB x 3 = 12.9 GB regardless of which/how many
// members are currently healthy; using the live member count instead
// produced a different (wrong) number depending on whether a stale
// (that) extra member was still attached or already removed.
// `member_states.faulty/spare` stay purely a health-display concern
// (`annotateMembers`/`diskMemberHealth`) -- they must never feed this math.
export const raidDisksForBand = (band: GroupBandStatus, arrays: ArrayStatus[]): number | null => (
    arrays.find(array => array.name === band.md_name)?.raid_disks ?? null
);

// `raidDisks` is the array's live, configured disk count (`raidDisksForBand`).
// `null` when it isn't known (the array isn't currently live/assembled, or
// its `raid_disks` itself reads `null`) -- never guessed from member counts.
export const computeBandCapacity = (band: GroupBandStatus, raidDisks: number | null): BandCapacity | null => {
    if (raidDisks === null || raidDisks <= 0)
        return null;
    const dataMembers = raidDataMembers(band.level, raidDisks);
    if (dataMembers === null)
        return null;
    const memberBytes = band.usable_bytes / dataMembers;
    const parityBytes = memberBytes * (raidDisks - dataMembers);
    return {
        memberCount: raidDisks, dataMembers, memberBytes, parityBytes, rawBytes: band.usable_bytes + parityBytes,
    };
};

// --- Per-member fault/spare annotation --------------------------------

export interface AnnotatedMember {
    name: string;
    faulty: boolean;
    spare: boolean;
    // Write_mostly/replacement were parsed into MemberStatus but
    // dropped here, so a member mid-`disk replace` (mdadm's live "(R)"
    // marker) read identically to a plain spare.
    write_mostly: boolean;
    replacement: boolean;
}

/** Pairs each raw member name with its live fault/spare/write-mostly/replacement state, if known. Mirrors `annotated_members` in `render.rs`. */
export const annotateMembers = (members: string[], memberStates: MemberStatus[] | undefined): AnnotatedMember[] => {
    const byName = new Map((memberStates ?? []).map(state => [state.name, state]));
    return members.map(name => {
        const state = byName.get(name);
        return {
            name,
            faulty: state?.faulty ?? false,
            spare: state?.spare ?? false,
            write_mostly: state?.write_mostly ?? false,
            replacement: state?.replacement ?? false,
        };
    });
};

// A partition name is the disk name plus a partition suffix (`sdc` -> `sdc1`,
// `nvme0n1`/`loop12` -> `nvme0n1p1`/`loop12p1`). Used only to decide whether
// a disk row should visibly flag a member as faulty/spare -- never for
// capacity math, which stays keyed by array/band membership.
const memberBelongsToDisk = (diskName: string, memberName: string): boolean => (
    memberName === diskName || new RegExp(`^${diskName.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}p?\\d+$`).test(memberName)
);

export interface DiskMemberHealth {
    faulty: boolean;
    spare: boolean;
    // See AnnotatedMember's identical fields -- a disk backing a live
    // replacement target must not read as a plain spare in the drive list.
    write_mostly: boolean;
    replacement: boolean;
}

/**
 * Whether any of `disk`'s partitions show up as a faulty/spare/write-mostly/
 * replacement member of one of its arrays (the drive-list requirement: a
 * disk backing a faulty member must not read as a plain healthy member).
 * Looks up each array named in `disk.arrays` and scans its `member_states`
 * for a name that matches this disk.
 */
export const diskMemberHealth = (disk: DiskStatus, arrays: ArrayStatus[]): DiskMemberHealth => {
    let faulty = false;
    let spare = false;
    let writeMostly = false;
    let replacement = false;
    for (const arrayName of disk.arrays) {
        const array = arrays.find(a => a.name === arrayName);
        for (const member of array?.member_states ?? []) {
            if (!memberBelongsToDisk(disk.name, member.name))
                continue;
            faulty = faulty || member.faulty;
            spare = spare || member.spare;
            writeMostly = writeMostly || member.write_mostly;
            replacement = replacement || member.replacement;
        }
    }
    return { faulty, spare, write_mostly: writeMostly, replacement };
};

// --- Storage allocation (capacity panel + allocation bar) -------------------

export interface AllocationSummary {
    /** Sum of every group's `usable_bytes` -- always known (state.toml). */
    usableBytes: number;
    /** Sum of computable bands' parity bytes; `null` if no band is live. */
    parityBytes: number | null;
    /** True when some (but not all) bands contributed to `parityBytes`. */
    parityBytesPartial: boolean;
    /** Same "unknown if any unknown" rule as `StatusSummary.rawBytes`. */
    rawDiskBytes: number | null;
    /** Sum of every band's raw bytes -- `null` unless ALL bands are live. */
    bandRawBytes: number | null;
    /**
     * Sum of every system disk's (`disk.system_disk`) size -- shr-rs
     * preflight refuses these outright, so they can never become
     * `unassignedBytes`. Still surfaced here (never hidden) so a
     * caller can render them as their own labelled segment instead of
     * silently dropping their bytes. `0` when there is no system disk;
     * `null` only if a system disk's own size is unknown.
     */
    systemDiskBytes: number | null;
    /**
     * Physical capacity not accounted for by any band's raw footprint --
     * e.g. a disk holding a smaller slice than its full size while a wider
     * band is still awaiting more members. Excludes system disks (see
     * `systemDiskBytes`). `null` unless both operands are fully known.
     */
    unassignedBytes: number | null;
}

export const summarizeAllocation = (report: StatusReport): AllocationSummary => {
    const bands = report.groups.flatMap(group => group.bands);
    const usableBytes = report.groups.reduce((sum, group) => sum + group.usable_bytes, 0);

    const capacities = bands.map(band => computeBandCapacity(band, raidDisksForBand(band, report.arrays)));
    const known = capacities.filter((capacity): capacity is BandCapacity => capacity !== null);

    const parityBytes = known.length > 0
        ? known.reduce((sum, capacity) => sum + capacity.parityBytes, 0)
        : null;
    const parityBytesPartial = known.length > 0 && known.length < bands.length;

    const bandRawBytes = known.length === bands.length && bands.length > 0
        ? known.reduce((sum, capacity) => sum + capacity.rawBytes, 0)
        : null;

    const rawDiskBytes = totalKnownBytes(report.disks.map(disk => disk.size));

    const systemDisks = report.disks.filter(disk => disk.system_disk);
    const systemDiskBytes = systemDisks.length > 0 ? totalKnownBytes(systemDisks.map(disk => disk.size)) : 0;

    // Only non-system disks can ever be "unassigned" -- shr-rs preflight
    // refuses a system disk outright, so counting its bytes here would tell
    // an operator they could add capacity they are not allowed to touch.
    const nonSystemRawDiskBytes = totalKnownBytes(
        report.disks.filter(disk => !disk.system_disk).map(disk => disk.size),
    );
    const unassignedBytes = nonSystemRawDiskBytes !== null && bandRawBytes !== null
        ? Math.max(nonSystemRawDiskBytes - bandRawBytes, 0)
        : null;

    return {
        usableBytes, parityBytes, parityBytesPartial, rawDiskBytes, bandRawBytes, systemDiskBytes, unassignedBytes,
    };
};

export interface CapacityUsage {
    usedBytes: number | null;
    freeBytes: number | null;
}

// `fsDf` is `null` on transport/parse failure (see app.tsx -- a failed `fs
// df` call degrades this panel, it never fails the whole dashboard). Even a
// successful call reads `null` for every used/total figure today (no Btrfs
// usage parser exists yet -- see `FsDfReport`'s doc comment), so on a real
// host this returns `{ usedBytes: null, freeBytes: null }` until that parser
// ships and a caller starts populating `FsUsageInput`.
export const summarizeCapacityUsage = (fsDf: FsDfReport | null, usableBytes: number): CapacityUsage => {
    if (!fsDf || fsDf.groups.length === 0)
        return { usedBytes: null, freeBytes: null };

    const perGroup = fsDf.groups.map(group => (
        group.data_used_bytes === null || group.metadata_used_bytes === null
            ? null
            : group.data_used_bytes + group.metadata_used_bytes
    ));
    // A partial reading (some groups measured, some not) still reports
    // `null` rather than summing only the known groups -- a partial sum
    // would understate total usage, not just "not know all of it".
    if (perGroup.some(value => value === null))
        return { usedBytes: null, freeBytes: null };

    const usedBytes = perGroup.reduce((sum: number, value) => sum + (value ?? 0), 0);
    return { usedBytes, freeBytes: Math.max(usableBytes - usedBytes, 0) };
};

export type AllocationSegmentKind = "used" | "free" | "unknown" | "parity" | "unassigned" | "system";

export interface AllocationSegment {
    kind: AllocationSegmentKind;
    bytes: number;
}

/**
 * The storage-allocation bar's segments, in draw order. Zero-byte segments
 * are omitted so a host with e.g. no unassigned capacity doesn't render a
 * degenerate sliver. When used/free can't be measured, a single "unknown"
 * segment stands in for the whole usable-capacity portion of the bar rather
 * than guessing a used/free split.
 */
export const buildAllocationSegments = (
    allocation: AllocationSummary,
    usage: CapacityUsage,
): AllocationSegment[] => {
    const segments: AllocationSegment[] = [];
    if (usage.usedBytes !== null && usage.freeBytes !== null) {
        if (usage.usedBytes > 0)
            segments.push({ kind: "used", bytes: usage.usedBytes });
        if (usage.freeBytes > 0)
            segments.push({ kind: "free", bytes: usage.freeBytes });
    } else if (allocation.usableBytes > 0) {
        segments.push({ kind: "unknown", bytes: allocation.usableBytes });
    }
    if (allocation.parityBytes !== null && allocation.parityBytes > 0)
        segments.push({ kind: "parity", bytes: allocation.parityBytes });
    if (allocation.unassignedBytes !== null && allocation.unassignedBytes > 0)
        segments.push({ kind: "unassigned", bytes: allocation.unassignedBytes });
    if (allocation.systemDiskBytes !== null && allocation.systemDiskBytes > 0)
        segments.push({ kind: "system", bytes: allocation.systemDiskBytes });
    return segments;
};

// --- Redundancy / duration / scrub formatting -------------------------------

/** Guaranteed simultaneous disk losses a redundancy mode survives (mirrors `RedundancyMode::fault_tolerance`). */
export const groupFaultTolerance = (mode: string): number | null => {
    switch (mode.trim().toLowerCase()) {
    case "shr":
        return 1;
    case "shr2":
        return 2;
    default:
        return null;
    }
};

export interface GroupToleranceStatus {
    /** What the mode promises when everything is healthy. `null` for an unrecognized mode. */
    nominal: number | null;
    /**
     * `nominal` minus the worst-affected band's live faulty-member count.
     * Can go negative -- a band already past its tolerance stays visibly
     * past it, never clamped to 0. `null` when `nominal` is `null`,
     * there are no bands, or ANY band lacks live `member_states` -- same
     * all-or-nothing rule as `totalKnownBytes`, since the one band with no
     * data could be the actual worst one.
     */
    remaining: number | null;
}

/**
 * Remaining (not nominal) disk-loss tolerance for a group. Bands can
 * differ in width/level (e.g. a 2-disk raid1 band next to a 4-disk raid6
 * band in the same SHR-2 group), so the group's remaining margin is driven
 * by whichever band currently has the most faulty members, not an average
 * or a sum across bands.
 */
export const groupToleranceStatus = (mode: string, bands: GroupBandStatus[]): GroupToleranceStatus => {
    const nominal = groupFaultTolerance(mode);
    if (nominal === null || bands.length === 0)
        return { nominal, remaining: null };

    const allLive = bands.every(band => band.member_states !== undefined);
    if (!allLive)
        return { nominal, remaining: null };

    const worstFaulty = Math.max(...bands.map(
        band => (band.member_states ?? []).filter(member => member.faulty).length,
    ));
    return { nominal, remaining: nominal - worstFaulty };
};

export const formatDuration = (minutes: number | null): string => {
    if (minutes === null)
        return "알 수 없음";
    const totalMin = Math.max(Math.round(minutes), 0);
    if (totalMin < 1)
        return "1분 미만";
    const hours = Math.floor(totalMin / 60);
    const mins = totalMin % 60;
    return hours > 0 ? `${hours}시간 ${mins}분` : `${mins}분`;
};

/**
 * The kernel's own word for what an array is doing (`/proc/mdstat`'s
 * reshape/recovery/resync/check/repair), in Korean. Unknown values fall
 * through verbatim rather than being hidden -- a state we cannot name is
 * still worth showing.
 */
const SYNC_ACTION_LABELS: Record<string, string> = {
    reshape: "용량 재구성",
    recovery: "복구",
    resync: "재동기화",
    check: "무결성 검사",
    repair: "복원",
};

export const describeSyncAction = (action: string): string =>
    SYNC_ACTION_LABELS[action.toLowerCase()] ?? action;

/** Same idea for `/proc/mdstat`'s array state word. */
const ARRAY_STATE_LABELS: Record<string, string> = {
    active: "동작 중",
    clean: "동작 중 (기록 없음)",
    inactive: "정지됨",
};

export const describeArrayState = (state: string): string =>
    ARRAY_STATE_LABELS[state.toLowerCase()] ?? state;

export const formatSyncProgress = (sync: SyncSummary | null): string => {
    if (!sync)
        return "유휴 (진행 중인 작업 없음)";
    const percent = sync.percent === null ? "진행률 알 수 없음" : `${sync.percent.toFixed(1)}%`;
    return `${describeSyncAction(sync.action)} · ${percent} · 남은 시간 ${formatDuration(sync.finish_min)}`;
};

/**
 * `panels.tsx`'s `ArrayRow` percent+ETA cell, pulled out of the JSX so
 * it's a plain function this file's own tests can assert on directly --
 * this exact fragment is the one that used to print raw minutes ("약
 * 540.0분") right next to `formatSyncProgress`'s correctly-formatted "9시간
 * 0분" for the same field. Deliberately not reusing `formatSyncProgress`
 * itself: `ArrayRow` renders `sync.action` separately (its own `<strong>`),
 * so this only covers the percent/ETA half.
 */
export const formatSyncPercentEta = (sync: SyncSummary | null): string => {
    if (!sync)
        return "";
    const percent = sync.percent === null ? "진행률 계산 중" : `${sync.percent.toFixed(1)}%`;
    const eta = sync.finish_min === null ? "" : ` · 약 ${formatDuration(sync.finish_min)}`;
    return `${percent}${eta}`;
};

export interface ScrubDisplay {
    text: string;
    tone: "good" | "warning" | "neutral";
}

export const formatScrub = (last: ScrubSummary | null, inProgress: boolean): ScrubDisplay => {
    if (inProgress)
        return { text: "스크럽 진행 중", tone: "neutral" };
    if (!last)
        return { text: "스크럽 이력 없음", tone: "neutral" };
    const outcomeLabel = last.outcome === "completed" ? "완료" : last.outcome === "cancelled" ? "취소됨" : "실패";
    const hasErrors = last.error_count > 0;
    const text = `${last.finished_at} ${outcomeLabel}` + (hasErrors ? ` · 오류 ${last.error_count}건` : "");
    return { text, tone: hasErrors || last.outcome === "failed" ? "warning" : "good" };
};

// --- Background RAID activity -----------------------------------------
// The dashboard's top summary strip had no card for background mdadm
// activity (resync/recovery/reshape/check) -- an operator had to expand the
// "mdadm 어레이 인벤토리" panel to notice a rebuild in progress, even though a
// `recovery` running means reduced/lost redundancy right now. Everything
// here is derived strictly from `ArrayStatus.sync` (the live mdstat-sourced
// signal parsed by `parseSync`), never from a group's `scrub_in_progress` or
// expansion flags in state.toml -- rule 2: trust kernel state, not recorded
// state.

/** mdadm's documented `sync_action` values; anything else passes through as `other` rather than being misclassified as one of these four. */
export type SyncKind = "recovery" | "reshape" | "resync" | "check" | "other";

export const classifySyncAction = (action: string): SyncKind => {
    switch (action.trim().toLowerCase()) {
    case "recovery":
        return "recovery";
    case "reshape":
        return "reshape";
    case "resync":
        return "resync";
    case "check":
        return "check";
    default:
        return "other";
    }
};

export interface ActiveSync {
    arrayName: string;
    action: string;
    kind: SyncKind;
    percent: number | null;
    finishMin: number | null;
}

/** Every array currently mid-sync, in `arrays` order. Idle arrays (`sync: null`) are excluded, never padded in as "0%". */
export const activeBackgroundSyncs = (arrays: ArrayStatus[]): ActiveSync[] => {
    const active: ActiveSync[] = [];
    for (const array of arrays) {
        if (array.sync === null)
            continue;
        active.push({
            arrayName: array.name,
            action: array.sync.action,
            kind: classifySyncAction(array.sync.action),
            percent: array.sync.percent,
            finishMin: array.sync.finish_min,
        });
    }
    return active;
};

// `recovery` (rebuild) outranks `reshape` (structural change) outranks
// `resync`/`check`/`other` (routine) -- so a rebuild on one array is never
// buried behind a routine scrub running on another when picking the
// headline kind for the summary card.
const SYNC_KIND_SEVERITY: Record<SyncKind, number> = {
    recovery: 3,
    reshape: 2,
    resync: 1,
    check: 1,
    other: 1,
};

// Never invent a friendlier label for a `sync.action` this UI doesn't
// recognize (mirrors `modeLabel` in panels.tsx) -- `other` falls back to the
// raw action string mdadm reported instead of a made-up noun.
const SYNC_KIND_LABEL: Record<Exclude<SyncKind, "other">, string> = {
    recovery: "복구",
    reshape: "확장",
    resync: "재동기화",
    check: "스크럽",
};

const syncKindLabel = (sync: ActiveSync): string => (
    sync.kind === "other" ? sync.action : SYNC_KIND_LABEL[sync.kind]
);

// Matches `ArrayRow`'s established wording in panels.tsx exactly (`진행률
// 계산 중`) -- mdadm itself can report a sync with no percent/ETA yet, and
// this must read as "unknown", never as a computed/guessed figure.
const formatActivePercent = (sync: ActiveSync): string => {
    const percentText = sync.percent === null ? "진행률 계산 중" : `${sync.percent.toFixed(1)}%`;
    // Route through formatDuration like formatSyncProgress does -- this
    // used to print raw minutes ("약 540.0분") one panel above the correctly
    // formatted "9시간 0분" for the same field.
    const etaText = sync.finishMin === null ? "" : ` · 약 ${formatDuration(sync.finishMin)}`;
    return `${percentText}${etaText}`;
};

export interface BackgroundActivityView {
    tone: "good" | "warning" | "neutral";
    headline: string;
    detail: string;
}

/**
 * Render-ready summary of live background RAID activity for the dashboard's
 * top summary strip. Idle (no array syncing) reads as calm/unremarkable --
 * `good` tone, no alarming empty state (an always-warning dashboard trains
 * operators to ignore it). Otherwise the headline/tone follow the single
 * most severe kind present (`recovery`/`reshape` are `warning`-tier because
 * they mean reduced/lost redundancy or a structural change in progress;
 * `resync`/`check`/`other` are routine and stay `neutral`), while `detail`
 * lists every active array's own reported figures individually -- multiple
 * arrays' percentages are never averaged into one invented number.
 *
 * `arrays === []` alone is ambiguous between "nothing configured" (a
 * fresh host with no groups -- idle/green is correct, nothing to verify) and
 * "state.toml expects bands but no live mdadm array answered" (measured live:
 * `umount` + `vgchange -an` + `mdadm --stop` with state.toml intact reports
 * `health: degraded`, `arrays: []`). The old code read both as "유휴", the
 * same defect an earlier fix already addressed one card below on BandRow's sync cell (see
 * that comment in panels.tsx) -- this is that same axis on the sibling path.
 * `groups` is what tells the two apart: only a live array count of zero
 * *while a band was expected* is "can't verify", not "nothing to verify".
 */
export const describeBackgroundActivity = (arrays: ArrayStatus[], groups: GroupStatus[]): BackgroundActivityView => {
    const active = activeBackgroundSyncs(arrays);
    if (active.length === 0) {
        const bandsExpected = groups.some(group => group.bands.length > 0);
        if (bandsExpected && arrays.length === 0) {
            // Same phrasing as BandRow's sync cell (panels.tsx) and same
            // tone as the header's "주의 필요" badge for `health: degraded`
            // (healthTone in panels.tsx) -- one voice for one underlying fact.
            return { tone: "warning", headline: "확인 불가", detail: "실시간 어레이 정보 없음" };
        }
        return { tone: "good", headline: "유휴", detail: "진행 중인 백그라운드 작업 없음" };
    }

    const worst = active.reduce((a, b) => (SYNC_KIND_SEVERITY[b.kind] > SYNC_KIND_SEVERITY[a.kind] ? b : a));
    const tone: BackgroundActivityView["tone"] = worst.kind === "recovery" || worst.kind === "reshape"
        ? "warning"
        : "neutral";
    const worstLabel = syncKindLabel(worst);
    const headline = active.length === 1
        ? `${worstLabel} 진행 중`
        : `${worstLabel} 외 ${active.length - 1}건 진행 중`;
    const detail = active
            .map(sync => `${sync.arrayName} ${syncKindLabel(sync)} · ${formatActivePercent(sync)}`)
            .join(" / ");

    return { tone, headline, detail };
};
