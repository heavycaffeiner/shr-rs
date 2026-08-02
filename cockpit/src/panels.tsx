/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Display components for the SHR-RS dashboard. Pure rendering only -- no
 * `cockpit.spawn`, no state machine, no action buttons (that's `app.tsx`'s
 * job for its own header, and `actions.ts`/`actionsDialogs.tsx`'s job for
 * anything destructive). Everything numeric shown here comes from a pure
 * `model.ts` function; nothing is invented client-side (see the design
 * for the exact list of values that genuinely cannot be filled yet, and what
 * CLI surface each one would need).
 */

import React from "react";

import {
    Card,
    CardBody,
    CardExpandableContent,
    CardHeader,
    CardTitle,
    Content,
    DescriptionList,
    DescriptionListDescription,
    DescriptionListGroup,
    DescriptionListTerm,
    Flex,
    FlexItem,
    Gallery,
    Progress,
    ProgressMeasureLocation,
    Split,
    SplitItem,
    Stack,
    StackItem,
    Title,
} from "@patternfly/react-core";
import { Table, Tbody, Td, Th, Thead, Tr } from "@patternfly/react-table";

import { Badge, Caveat, CellSub, Chip, MONO, MetricCard, Muted, type Tone } from "./ui.js";
import {
    type ArrayStatus,
    type DiskStatus,
    type FsDfReport,
    type GroupBandStatus,
    type GroupStatus,
    type Health,
    type MemberStatus,
    type SmartState,
    type StatusReport,
    annotateMembers,
    arrayNeedsAttention,
    buildAllocationSegments,
    computeBandCapacity,
    describeArrayState,
    describeSyncAction,
    diskMemberHealth,
    formatBytes,
    formatScrub,
    formatSyncPercentEta,
    formatSyncProgress,
    groupToleranceStatus,
    raidDisksForBand,
    summarizeAllocation,
    summarizeCapacityUsage,
    summarizeStatus,
    type GroupToleranceStatus,
} from "./model.ts";

// `Tone`/`Badge` moved to `ui.tsx` when the hand-written `.status-badge`
// stylesheet was replaced by PatternFly's `Label`. Re-exported here because
// several call sites (and `app.tsx`) have always imported them from this
// module, and the tone vocabulary is still this file's to define.
export type { Tone };
export { Badge };

export const healthTone = (health: Health): Tone => {
    switch (health) {
    case "healthy":
        return { label: "정상", tone: "good" };
    case "degraded":
        return { label: "주의 필요", tone: "warning" };
    default:
        return { label: "어레이 없음", tone: "neutral" };
    }
};

const smartTone = (state: SmartState): Tone => {
    switch (state) {
    case "ok":
        return { label: "정상", tone: "good" };
    case "warning":
        return { label: "경고", tone: "warning" };
    default:
        return { label: "알 수 없음", tone: "neutral" };
    }
};

const EmptyRow = ({ columns, message }: { columns: number; message: string }) => (
    <Tr>
        <Td colSpan={columns}><Muted>{message}</Muted></Td>
    </Tr>
);

/** The accordion every inventory panel is wrapped in. `details`/`summary`
 * with a hand-drawn marker became PatternFly's expandable `Card`, which is
 * what Cockpit's own pages use for exactly this. `isExpanded` is local state
 * because the old `<details open>` was uncontrolled too. */
const Section = (
    { title, note, defaultExpanded = true, children }: {
        title: string;
        note: React.ReactNode;
        defaultExpanded?: boolean;
        children: React.ReactNode;
    },
) => {
    const [expanded, setExpanded] = React.useState(defaultExpanded);
    return (
        <Card id={`section-${title}`} isExpanded={expanded} component="div">
            <CardHeader onExpand={() => setExpanded(value => !value)}>
                <CardTitle>
                    <Split hasGutter>
                        <SplitItem isFilled>{title}</SplitItem>
                        <SplitItem className="pf-v6-u-font-size-sm pf-v6-u-text-color-subtle">{note}</SplitItem>
                    </Split>
                </CardTitle>
            </CardHeader>
            <CardExpandableContent>
                <CardBody>{children}</CardBody>
            </CardExpandableContent>
        </Card>
    );
};

// Write_mostly/replacement were parsed but never rendered, so a member
// mid-`disk replace` (mdadm's live "(R)" replacement-in-progress marker) was
// visually identical to an ordinary spare. Precedence mirrors severity:
// faulty (kicked) outranks replacement (actively rebuilding) outranks spare
// (idle, unused) outranks write_mostly (a steady-state config flag, not a
// health concern) -- letters match mdadm's own `/proc/mdstat` tags exactly.
type MemberFlags = { faulty: boolean; spare: boolean; write_mostly: boolean; replacement: boolean };

type ChipColor = "grey" | "red" | "orange" | "blue" | "purple";

const memberChipColor = (state: MemberFlags): ChipColor => (
    state.faulty
        ? "red"
        : state.replacement
            ? "purple"
            : state.spare
                ? "blue"
                : state.write_mostly
                    ? "orange"
                    : "grey"
);

const memberSuffix = (state: MemberFlags): string => (
    state.faulty
        ? "(F)"
        : state.replacement
            ? "(R)"
            : state.spare
                ? "(S)"
                : state.write_mostly
                    ? "(W)"
                    : ""
);

// Renders a raw member-name list with faulty/spare devices visibly marked
// -- the plain `members.join(", ")` this replaces made a kicked
// (`(F)`) member indistinguishable from a healthy one.
const MemberList = ({
    members, memberStates, emptyLabel,
}: { members: string[]; memberStates: MemberStatus[] | undefined; emptyLabel?: string }) => {
    if (members.length === 0)
        return emptyLabel ? <Muted>{emptyLabel}</Muted> : null;

    return (
        <Flex spaceItems={{ default: "spaceItemsXs" }} className="pf-v6-u-mt-xs">
            {annotateMembers(members, memberStates).map((member, i) => (
                <FlexItem key={`${member.name}-${i}`}>
                    <Chip color={memberChipColor(member)}>
                        {member.name}{memberSuffix(member)}
                    </Chip>
                </FlexItem>
            ))}
        </Flex>
    );
};

const DiskRow = ({ disk, arrays }: { disk: DiskStatus; arrays: ArrayStatus[] }) => {
    const smart = smartTone(disk.smart.state);
    const memberHealth = diskMemberHealth(disk, arrays);
    const smartDetails = [
        disk.smart.temperature_c === null ? null : `${disk.smart.temperature_c}°C`,
        disk.smart.power_on_hours === null ? null : `${disk.smart.power_on_hours.toLocaleString()}시간`,
        disk.smart.pending_sectors ? `보류 섹터 ${disk.smart.pending_sectors}` : null,
        disk.smart.reallocated_sectors ? `재할당 ${disk.smart.reallocated_sectors}` : null,
        disk.smart.uncorrectable_sectors ? `복구 불가 ${disk.smart.uncorrectable_sectors}` : null,
        disk.smart.nvme_critical_warning ? `NVMe 경고 0x${disk.smart.nvme_critical_warning.toString(16)}` : null,
    ].filter(Boolean).join(" · ");

    return (
        <Tr>
            <Td dataLabel="노드">
                <strong className={MONO}>/dev/{disk.name}</strong>
                <CellSub>{disk.rotational === false ? "SSD / NVMe" : "회전식 디스크"}</CellSub>
                {disk.id && <CellSub className={MONO}>{disk.id}</CellSub>}
                {disk.system_disk && (
                    <CellSub>
                        <Badge tone={{ label: "시스템 디스크 (RAID 대상 아님)", tone: "warning" }} />
                        {disk.system_mounts && disk.system_mounts.length > 0 && (
                            <CellSub className={MONO}>{disk.system_mounts.join(", ")}</CellSub>
                        )}
                    </CellSub>
                )}
            </Td>
            <Td dataLabel="모델 / 일련번호">
                {disk.model ?? "모델 정보 없음"}
                <CellSub className={MONO}>{disk.serial ?? "일련번호 없음"}</CellSub>
            </Td>
            <Td dataLabel="용량" className={MONO}>{formatBytes(disk.size)}</Td>
            <Td dataLabel="SMART 상태 / 온도 / 가동시간">
                <Badge tone={smart} />
                {smartDetails && <CellSub>{smartDetails}</CellSub>}
            </Td>
            <Td dataLabel="연결 어레이">
                {disk.arrays.length > 0
                    ? (
                        <>
                            <Flex spaceItems={{ default: "spaceItemsXs" }}>
                                {disk.arrays.map(name => (
                                    <FlexItem key={name}>
                                        <Chip color={memberChipColor(memberHealth)}>
                                            {name}{memberSuffix(memberHealth)}
                                        </Chip>
                                    </FlexItem>
                                ))}
                            </Flex>
                            {memberHealth.faulty && <CellSub>이 디스크의 파티션이 어레이에서 faulty로 표시되었습니다.</CellSub>}
                        </>
                    )
                    : <Muted>RAID 미연결</Muted>}
            </Td>
        </Tr>
    );
};

export const DrivesPanel = ({ report, warningDisks }: { report: StatusReport; warningDisks: number }) => (
    <Section title="구성 드라이브 목록" note={`${report.disks.length}개 · SMART 경고 ${warningDisks}개`}>
        <Table variant="compact" aria-label="구성 드라이브 목록">
            <Thead>
                <Tr>
                    <Th>노드</Th>
                    <Th>모델 / 일련번호</Th>
                    <Th>용량</Th>
                    <Th>SMART 상태 / 온도 / 가동시간</Th>
                    <Th>연결 어레이</Th>
                </Tr>
            </Thead>
            <Tbody>
                {report.disks.length > 0
                    ? report.disks.map(disk => <DiskRow disk={disk} arrays={report.arrays} key={disk.name} />)
                    : <EmptyRow columns={5} message="감지된 물리 디스크가 없습니다." />}
            </Tbody>
        </Table>
    </Section>
);

const ArrayRow = ({ array }: { array: ArrayStatus }) => {
    const active = array.active_disks ?? array.members.length;
    const expected = array.raid_disks ?? array.members.length;
    const isInvalidRaid6 = array.level?.toLowerCase() === "raid6" && expected < 4;
    const tone: Tone = arrayNeedsAttention(array)
        ? { label: "주의", tone: "warning" }
        : { label: "정상", tone: "good" };

    return (
        <Tr>
            <Td dataLabel="mdadm 디바이스"><strong className={MONO}>/dev/{array.name}</strong></Td>
            <Td dataLabel="RAID 레벨" className={MONO}>{array.level?.toUpperCase() ?? "알 수 없음"}</Td>
            <Td dataLabel="활성 / 목표 멤버">
                <span className={MONO}>{active}/{expected}</span>
                <MemberList members={array.members} memberStates={array.member_states} emptyLabel="멤버 정보 없음" />
            </Td>
            <Td dataLabel="동기화">
                {array.sync
                    ? (
                        <>
                            <strong>{describeSyncAction(array.sync.action)}</strong>
                            {/* `formatSyncPercentEta` (model.ts) routes the ETA through
                                `formatDuration`, same as `formatSyncProgress` used one panel
                                below for bands -- this used to print raw minutes ("약
                                540.0분") next to a correctly-formatted duration, and is now a
                                plain function `model.test.ts` asserts on directly instead of
                                inline JSX with no test surface. */}
                            <CellSub>{formatSyncPercentEta(array.sync)}</CellSub>
                        </>
                    )
                    : <Muted>유휴</Muted>}
            </Td>
            <Td dataLabel="상태">
                <Badge tone={tone} />
                <CellSub>
                    {[
                        describeArrayState(array.state),
                        array.read_only ? "읽기 전용" : null,
                        array.degraded ? "성능 저하" : null,
                        isInvalidRaid6 ? "RAID6는 최소 4개 멤버 필요" : null,
                    ].filter(Boolean).join(" · ")}
                </CellSub>
            </Td>
        </Tr>
    );
};

export const ArraysPanel = ({ report }: { report: StatusReport }) => (
    <Section title="mdadm 어레이 인벤토리" note={`${report.arrays.length}개 어레이 (실시간 조회)`}>
        <Table variant="compact" aria-label="mdadm 어레이 인벤토리">
            <Thead>
                <Tr>
                    <Th>mdadm 디바이스</Th>
                    <Th>RAID 레벨</Th>
                    <Th>활성 / 목표 멤버</Th>
                    <Th>동기화</Th>
                    <Th>상태</Th>
                </Tr>
            </Thead>
            <Tbody>
                {report.arrays.length > 0
                    ? report.arrays.map(array => <ArrayRow array={array} key={array.name} />)
                    : <EmptyRow columns={5} message="구성된 mdadm 어레이가 없습니다." />}
            </Tbody>
        </Table>
    </Section>
);

const modeLabel = (mode: string): string => {
    switch (mode.trim().toLowerCase()) {
    case "shr":
        return "SHR";
    case "shr2":
        return "SHR-2";
    default:
        // Never invent a friendlier label for a mode this UI doesn't
        // recognize -- show exactly what state.toml/the CLI reported so a
        // future third mode is visible, not silently mislabeled.
        return mode;
    }
};

// The fault-tolerance card used to show the mode's nominal tolerance
// unconditionally -- a group already missing a disk read identically to a
// fully healthy one. These two helpers turn `groupToleranceStatus`'s
// {nominal, remaining} into the label/tone actually shown, and are the only
// place that decides the wording -- the arithmetic itself stays in model.ts.
const toleranceLabel = ({ nominal, remaining }: GroupToleranceStatus): string => {
    if (nominal === null)
        return "허용 손실 수 알 수 없는 모드";
    if (remaining === null)
        return `설계상 ${nominal}디스크 손실 허용 (실시간 멤버 정보 없음)`;
    if (remaining === nominal)
        return `${nominal}디스크 손실 허용`;
    if (remaining >= 0)
        return `남은 여유 ${remaining} / 설계상 ${nominal}디스크 손실 허용`;
    // Never clamp to 0 -- a band already beyond its tolerance must read as
    // beyond it, not as "no margin left" (which would understate the risk).
    return `허용 한도 초과 (설계상 ${nominal}디스크 손실 허용)`;
};

const toleranceTone = ({ nominal, remaining }: GroupToleranceStatus): Tone["tone"] => {
    if (nominal === null)
        return "neutral";
    // Unknown remaining is never "good" -- the one band with no live data
    // could be the one that's actually degraded (see groupToleranceStatus).
    if (remaining === null || remaining < nominal)
        return "warning";
    return "good";
};

const BandRow = ({ band, arrays }: { band: GroupBandStatus; arrays: ArrayStatus[] }) => {
    // Geometry comes from the array's configured raid_disks, not from
    // any live/healthy member count -- see computeBandCapacity's doc comment.
    const capacity = computeBandCapacity(band, raidDisksForBand(band, arrays));
    const scrub = formatScrub(band.last_scrub, band.scrub_in_progress);

    return (
        <Tr>
            <Td dataLabel="밴드" className={MONO}>band{band.index}</Td>
            <Td dataLabel="RAID 레벨" className={MONO}>{band.level.toUpperCase()}</Td>
            <Td dataLabel="mdadm 디바이스 / UUID">
                <span className={MONO}>/dev/{band.md_name}</span>
                <CellSub className={MONO}>{band.md_uuid ?? "UUID 알 수 없음"}</CellSub>
            </Td>
            <Td dataLabel="참여 멤버 및 슬라이스 크기">
                {band.members.length > 0
                    ? (
                        <>
                            <MemberList members={band.members} memberStates={band.member_states} />
                            {capacity && (
                                <CellSub>
                                    <span title="usable_bytes와 RAID 레벨/구성된 디스크 수(raid_disks)로부터 계산한 값입니다. 멤버 고장 여부는 이 값에 영향을 주지 않습니다.">
                                        슬라이스 {formatBytes(capacity.memberBytes)} × {capacity.memberCount}개 디스크
                                    </span>
                                </CellSub>
                            )}
                        </>
                    )
                    : <Muted>실시간 멤버 정보 없음</Muted>}
            </Td>
            <Td dataLabel="가용 / 총 물리 용량" className={MONO}>
                {formatBytes(band.usable_bytes)} / {capacity ? formatBytes(capacity.rawBytes) : "알 수 없음"}
            </Td>
            <Td dataLabel="동기화 / 스크럽 상태">
                {band.resize_pending && <Badge tone={{ label: "확장 마무리 대기", tone: "warning" }} />}
                {/* `band.sync === null` covers BOTH "live array, nothing
                    syncing" and "no live mdadm array with this md_name at all"
                    -- see GroupBandStatus::sync's doc comment. Reading
                    `band.members.length === 0` (the same "no live array"
                    signal the member cell above already uses) first, exactly
                    like render.rs's render_band_detail_row/watch_band_row do
                    for this identical field, is what tells the two apart
                    instead of both silently reading as "유휴" (idle). Scrub
                    (below) is intentionally NOT gated the same way: it's
                    state.toml history, not a live-mdstat read, so it stays
                    meaningful even with no live array -- matches
                    render_band_detail_row, which doesn't gate scrub either. */}
                <CellSub>
                    {band.members.length === 0
                        ? "실시간 어레이 정보 없음"
                        : band.sync ? formatSyncProgress(band.sync) : "유휴"}
                </CellSub>
                <CellSub><Badge tone={{ label: scrub.text, tone: scrub.tone }} /></CellSub>
            </Td>
        </Tr>
    );
};

const GroupCard = ({ group, arrays }: { group: GroupStatus; arrays: ArrayStatus[] }) => {
    const scrubWarnings = group.bands.filter(
        band => band.scrub_in_progress || (band.last_scrub && band.last_scrub.error_count > 0),
    ).length;

    return (
        <Card isCompact component="div">
            <CardTitle>
                <Split hasGutter>
                    <SplitItem isFilled>
                        <Flex spaceItems={{ default: "spaceItemsSm" }} alignItems={{ default: "alignItemsCenter" }}>
                            <FlexItem><strong className={MONO}>{group.name}</strong></FlexItem>
                            <FlexItem><Chip>{modeLabel(group.mode)}</Chip></FlexItem>
                            {group.resize_pending && (
                                <FlexItem><Badge tone={{ label: "확장 마무리 대기", tone: "warning" }} /></FlexItem>
                            )}
                            {scrubWarnings > 0 && (
                                <FlexItem>
                                    <Badge tone={{ label: `스크럽 주의 밴드 ${scrubWarnings}개`, tone: "warning" }} />
                                </FlexItem>
                            )}
                        </Flex>
                    </SplitItem>
                    <SplitItem className={MONO}>{formatBytes(group.usable_bytes)}</SplitItem>
                </Split>
            </CardTitle>
            <CardBody>
                <DescriptionList isHorizontal isCompact className="pf-v6-u-mb-md">
                    <DescriptionListGroup>
                        <DescriptionListTerm>마운트 지점</DescriptionListTerm>
                        <DescriptionListDescription className={MONO}>{group.mount_point}</DescriptionListDescription>
                    </DescriptionListGroup>
                    {/* No layout version here: it is an internal on-disk
                        revision number that means nothing to whoever is
                        reading this card. */}
                    <DescriptionListGroup>
                        <DescriptionListTerm>파일시스템 UUID</DescriptionListTerm>
                        <DescriptionListDescription className={MONO}>
                            {group.fs_uuid ?? "알 수 없음"}
                        </DescriptionListDescription>
                    </DescriptionListGroup>
                    <DescriptionListGroup>
                        <DescriptionListTerm>구성 디스크</DescriptionListTerm>
                        <DescriptionListDescription>
                            {group.disks.length > 0
                                ? (
                                    <Flex spaceItems={{ default: "spaceItemsXs" }}>
                                        {group.disks.map(id => <FlexItem key={id}><Chip>{id}</Chip></FlexItem>)}
                                    </Flex>
                                )
                                : <Muted>없음</Muted>}
                        </DescriptionListDescription>
                    </DescriptionListGroup>
                </DescriptionList>
                <Table variant="compact" aria-label={`${group.name} 밴드 구성`}>
                    <Thead>
                        <Tr>
                            <Th>밴드</Th>
                            <Th>RAID 레벨</Th>
                            <Th>mdadm 디바이스 / UUID</Th>
                            <Th>참여 멤버 및 슬라이스 크기</Th>
                            <Th>가용 / 총 물리 용량</Th>
                            <Th>동기화 / 스크럽 상태</Th>
                        </Tr>
                    </Thead>
                    <Tbody>
                        {group.bands.length > 0
                            ? group.bands.map(band => <BandRow band={band} arrays={arrays} key={band.index} />)
                            : <EmptyRow columns={6} message="구성된 밴드가 없습니다." />}
                    </Tbody>
                </Table>
            </CardBody>
        </Card>
    );
};

export const GroupsPanel = ({ report }: { report: StatusReport }) => (
    <Section
        title="SHR 그룹"
        note={`${report.groups.length}개${report.groups.some(g => g.resize_pending) ? " · 확장 마무리 대기 중인 그룹 있음" : ""}`}
    >
        <Stack hasGutter>
            {report.groups.length > 0
                ? report.groups.map(group => (
                    <StackItem key={group.name}>
                        <GroupCard group={group} arrays={report.arrays} />
                    </StackItem>
                ))
                : (
                    <StackItem>
                        <Caveat>
                            아직 만들어진 SHR 그룹이 없습니다. 아래 목록은 이 서버에서 지금 감지된
                            디스크와 RAID 어레이입니다.
                        </Caveat>
                    </StackItem>
                )}
        </Stack>
    </Section>
);

// --- Capacity overview (mockup: top metrics-grid + allocation bar) ---------

const SEGMENT_LABEL: Record<string, string> = {
    used: "사용 중",
    free: "여유 공간",
    unknown: "가용 용량 (사용/여유 측정 불가)",
    parity: "패리티 보호",
    unassigned: "미할당 물리 용량 (RAID 추가 가능)",
    system: "시스템 디스크 (RAID 대상 아님)",
};

export const CapacityOverviewPanel = ({ report, fsDf }: { report: StatusReport; fsDf: FsDfReport | null }) => {
    const allocation = summarizeAllocation(report);
    const usage = summarizeCapacityUsage(fsDf, allocation.usableBytes);
    const segments = buildAllocationSegments(allocation, usage);
    const total = segments.reduce((sum, segment) => sum + segment.bytes, 0);

    // Remaining, not nominal, tolerance -- see toleranceLabel/toleranceTone.
    const tolerances = report.groups.map(group => (
        { name: group.name, mode: group.mode, status: groupToleranceStatus(group.mode, group.bands) }
    ));

    // `fs df --json` now has a live Btrfs usage parser (see model.ts's
    // FsDfReport doc comment) -- `usage.usedBytes` is only `null` when the
    // call itself failed, returned no groups, or a real group's figure is
    // genuinely missing (unmounted, `btrfs`/`df` error), never because "no
    // parser exists yet".
    const usedUnknownReason = fsDf === null
        ? "사용량을 조회하지 못했습니다 (shr-rs 버전이 낮거나 연결 문제일 수 있습니다)"
        : fsDf.groups.length === 0
            ? "구성된 그룹이 없습니다"
            : "일부 그룹의 사용량을 읽지 못했습니다 (마운트되어 있는지 확인하세요)";

    return (
        <>
            <Gallery hasGutter minWidths={{ default: "220px" }} aria-label="그룹 용량 개요">
                <MetricCard
                    label="가용 스토리지"
                    value={formatBytes(allocation.usableBytes)}
                    sub={`물리 용량 ${formatBytes(allocation.rawDiskBytes)}`}
                />
                <MetricCard
                    label="사용 중 공간"
                    value={usage.usedBytes === null ? "측정 불가" : formatBytes(usage.usedBytes)}
                    sub={usage.usedBytes === null ? usedUnknownReason : "마운트된 파일시스템 실측값"}
                />
                <MetricCard
                    label="여유 공간"
                    value={usage.freeBytes === null ? "측정 불가" : formatBytes(usage.freeBytes)}
                    sub={usage.freeBytes === null ? "사용 중 공간을 모르면 계산할 수 없습니다" : "할당 가능"}
                />
                <MetricCard
                    label="보호 레벨"
                    value={allocation.parityBytes === null
                        ? "측정 불가"
                        : `패리티 ${formatBytes(allocation.parityBytes)}`}
                    sub={tolerances.length > 0
                        ? (
                            <Stack>
                                {tolerances.map(t => (
                                    <StackItem key={t.name}>
                                        <Badge tone={{
                                            label: `${t.name}: ${modeLabel(t.mode)}`,
                                            tone: toleranceTone(t.status),
                                        }}
                                        />
                                        {" "}{toleranceLabel(t.status)}
                                    </StackItem>
                                ))}
                                {allocation.parityBytesPartial && <StackItem>(일부 밴드는 실시간 정보 없음)</StackItem>}
                            </Stack>
                        )
                        : "구성된 그룹 없음"}
                />
            </Gallery>

            <Card component="div" aria-label="스토리지 할당 현황">
                <CardTitle>
                    <Split hasGutter>
                        <SplitItem isFilled>스토리지 할당 현황</SplitItem>
                        <SplitItem className={MONO}>
                            {report.groups.map(g => modeLabel(g.mode)).join(", ") || "그룹 없음"}
                        </SplitItem>
                    </Split>
                </CardTitle>
                <CardBody>
                    {total > 0
                        ? (
                            <>
                                {/* The hand-drawn `.bar-container`/`.bar-seg` stack is now one
                                    PatternFly `Progress` per segment. A single multi-colour bar
                                    has no PatternFly equivalent, and stacked `Progress` rows are
                                    what its own storage examples use -- each segment keeps its
                                    exact percentage and its byte figure, which is what the bar
                                    was actually communicating. */}
                                <Stack hasGutter>
                                    {segments.map(segment => (
                                        <StackItem key={segment.kind}>
                                            <Progress
                                                value={(segment.bytes / total) * 100}
                                                title={`${SEGMENT_LABEL[segment.kind]} (${formatBytes(segment.bytes)})`}
                                                measureLocation={ProgressMeasureLocation.outside}
                                                variant={segment.kind === "used" ? "success" : undefined}
                                                aria-label={SEGMENT_LABEL[segment.kind]}
                                            />
                                        </StackItem>
                                    ))}
                                </Stack>
                                {allocation.systemDiskBytes !== null && allocation.systemDiskBytes > 0 && (
                                    <Caveat>
                                        시스템 디스크는 OS가 설치된 디스크입니다. 안전을 위해 RAID 후보에서 자동으로
                                        제외되며, 이 디스크의 용량은 미할당 물리 용량에 들어가지 않습니다.
                                    </Caveat>
                                )}
                            </>
                        )
                        : <Caveat>표시할 용량 정보가 없습니다.</Caveat>}
                </CardBody>
            </Card>
        </>
    );
};

// --- Storage stack / filesystem technical spec (mockup accordion 3) -------

const KvItem = ({ label, value, hint }: { label: string; value: string; hint?: string }) => (
    <DescriptionListGroup>
        <DescriptionListTerm>
            {label}
            {hint && <span title={hint}> ⓘ</span>}
        </DescriptionListTerm>
        <DescriptionListDescription className={MONO}>{value}</DescriptionListDescription>
    </DescriptionListGroup>
);

const UNKNOWN = "알 수 없음";
// The status report carries the configuration file's path, so this only fires
// when the backend genuinely does not know it (older CLI build).
const STATE_PATH_HINT = "이 버전의 shr-rs는 구성 파일 경로를 알려주지 않습니다.";

const TechSpecCard = ({ group, statePath }: { group: GroupStatus; statePath: string | null }) => (
    <DescriptionList isHorizontal isCompact>
        <KvItem label="그룹 이름" value={group.name} />
        <KvItem label="볼륨 그룹 (LVM VG)" value={group.vg_name ?? UNKNOWN} />
        <KvItem label="논리 볼륨 (LVM LV) / 마운트 위치" value={`${group.lv_name ?? UNKNOWN} → ${group.mount_point}`} />
        <KvItem label="파일시스템 UUID" value={group.fs_uuid ?? UNKNOWN} />
        <KvItem label="압축 방식 및 마운트 옵션" value={group.compression ?? UNKNOWN} />
        {statePath === null
            ? (
                <KvItem
                    label="구성 파일"
                    value={UNKNOWN}
                    hint={STATE_PATH_HINT}
                />
            )
            : (
                <KvItem
                    label="구성 파일"
                    value={statePath}
                />
            )}
    </DescriptionList>
);

export const TechSpecPanel = ({ report }: { report: StatusReport }) => (
    <Section title="볼륨 및 파일시스템 상세" note="LVM · Btrfs 구성" defaultExpanded={false}>
        <Stack hasGutter>
            {report.groups.length > 0
                ? report.groups.map(group => (
                    <StackItem key={group.name}>
                        <Title headingLevel="h3" size="md" className={MONO}>{group.name}</Title>
                        <TechSpecCard group={group} statePath={report.state_path} />
                    </StackItem>
                ))
                : <StackItem><Caveat>구성된 SHR 그룹이 없습니다.</Caveat></StackItem>}
        </Stack>
    </Section>
);

// --- Capacity methodology (kept separate from the operator-facing panel
// above -- this is the "how was this number derived / what's missing"
// explanation, deliberately closed by default). ------------------------------

export const CapacityMethodologyPanel = ({ report }: { report: StatusReport }) => {
    const summary = summarizeStatus(report);
    const allocation = summarizeAllocation(report);

    return (
        <Section title="용량 산정 방법" note="각 수치를 어떻게 구했는지" defaultExpanded={false}>
            <DescriptionList isHorizontal isCompact className="pf-v6-u-mb-md">
                <DescriptionListGroup>
                    <DescriptionListTerm>감지된 원시 용량</DescriptionListTerm>
                    <DescriptionListDescription className={MONO}>
                        {formatBytes(summary.rawBytes)}
                    </DescriptionListDescription>
                </DescriptionListGroup>
                <DescriptionListGroup>
                    <DescriptionListTerm>그룹 가용 용량 (구성 기록 합계)</DescriptionListTerm>
                    <DescriptionListDescription className={MONO}>
                        {formatBytes(allocation.usableBytes)}
                    </DescriptionListDescription>
                </DescriptionListGroup>
                <DescriptionListGroup>
                    <DescriptionListTerm>계산된 패리티 용량</DescriptionListTerm>
                    <DescriptionListDescription className={MONO}>
                        {formatBytes(allocation.parityBytes)}
                    </DescriptionListDescription>
                </DescriptionListGroup>
            </DescriptionList>
            <Content component="p">
                <strong>가용 용량</strong>은 그룹을 만들 때 기록해 둔 값을 그대로 더한 것이라 항상 표시됩니다.
                <strong> 패리티 용량</strong>은 각 RAID 어레이의 구성 디스크 수와 RAID 레벨로부터 이 화면이
                계산합니다. 어레이가 현재 조립되어 있지 않으면 계산할 수 없어 &quot;측정 불가&quot;로 표시됩니다.
                멤버 하나가 고장으로 표시되어도 어레이가 차지하는 디스크 수는 그대로이므로 이 값은 바뀌지
                않으며, 고장/예비 표시는 멤버 목록과 상태 배지에만 나타납니다. 계산값이라 실제 크기와 아주
                근소하게 다를 수 있습니다.
                <strong> 사용 중 공간</strong>은 마운트된 파일시스템에서 직접 읽은 실측값입니다. 마운트되어
                있지 않거나 조회에 실패하면 추정하지 않고 &quot;측정 불가&quot;로 둡니다.
                <strong> 감지된 원시 용량</strong>에는 OS가 설치된 시스템 디스크도 합산되지만, 그 디스크는
                RAID 후보에서 제외되므로 미할당 물리 용량에서는 빠집니다.
            </Content>
        </Section>
    );
};
