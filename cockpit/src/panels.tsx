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

import { _, format, ngettext } from "./i18n.js";
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
        return { label: _("panels.health.healthy"), tone: "good" };
    case "degraded":
        return { label: _("panels.health.degraded"), tone: "warning" };
    default:
        return { label: _("panels.health.none"), tone: "neutral" };
    }
};

const smartTone = (state: SmartState): Tone => {
    switch (state) {
    case "ok":
        return { label: _("common.ok"), tone: "good" };
    case "warning":
        return { label: _("panels.smart.warning"), tone: "warning" };
    default:
        return { label: _("common.unknown"), tone: "neutral" };
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
                    {/* `isWrappable`: Split declares no `flex-wrap`, so it takes
                        the initial `nowrap` and this heading and its note can
                        only compress against each other rather than reflow.
                        See app.tsx's card title for the measurement; same
                        reasoning at every card title in this file. */}
                    <Split hasGutter isWrappable>
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
        disk.smart.power_on_hours === null
            ? null
            : format(_("panels.smart.powerOnHours"), disk.smart.power_on_hours.toLocaleString()),
        disk.smart.pending_sectors ? format(_("panels.smart.pendingSectors"), disk.smart.pending_sectors) : null,
        disk.smart.reallocated_sectors ? format(_("panels.smart.reallocated"), disk.smart.reallocated_sectors) : null,
        disk.smart.uncorrectable_sectors ? format(_("panels.smart.uncorrectable"), disk.smart.uncorrectable_sectors) : null,
        disk.smart.nvme_critical_warning
            ? format(_("panels.smart.nvmeWarning"), disk.smart.nvme_critical_warning.toString(16))
            : null,
    ].filter(Boolean).join(" · ");

    return (
        <Tr>
            <Td dataLabel={_("panels.drives.col.node")}>
                <strong className={MONO}>/dev/{disk.name}</strong>
                <CellSub>{disk.rotational === false ? "SSD / NVMe" : _("panels.drives.spinning")}</CellSub>
                {disk.id && <CellSub className={MONO}>{disk.id}</CellSub>}
                {disk.system_disk && (
                    <CellSub>
                        <Badge tone={{ label: _("common.systemDisk"), tone: "warning" }} />
                        {disk.system_mounts && disk.system_mounts.length > 0 && (
                            <CellSub className={MONO}>{disk.system_mounts.join(", ")}</CellSub>
                        )}
                    </CellSub>
                )}
            </Td>
            <Td dataLabel={_("panels.drives.col.model")}>
                {disk.model ?? _("panels.drives.noModel")}
                <CellSub className={MONO}>{disk.serial ?? _("panels.drives.noSerial")}</CellSub>
            </Td>
            <Td dataLabel={_("panels.drives.col.capacity")} className={MONO}>{formatBytes(disk.size)}</Td>
            <Td dataLabel={_("panels.drives.col.smart")}>
                <Badge tone={smart} />
                {smartDetails && <CellSub>{smartDetails}</CellSub>}
            </Td>
            <Td dataLabel={_("panels.drives.col.arrays")}>
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
                            {memberHealth.faulty && (
                                <CellSub>{_("panels.drives.faultyPartition")}</CellSub>
                            )}
                        </>
                    )
                    : <Muted>{_("panels.drives.notInRaid")}</Muted>}
            </Td>
        </Tr>
    );
};

export const DrivesPanel = ({ report, warningDisks }: { report: StatusReport; warningDisks: number }) => (
    <Section
        title={_("panels.drives.title")}
        note={[
            format(ngettext("common.driveCount.one", "common.driveCount.other", report.disks.length), report.disks.length),
            format(ngettext("common.smartWarningCount.one", "common.smartWarningCount.other", warningDisks), warningDisks),
        ].join(" · ")}
    >
        <Table variant="compact" aria-label={_("panels.drives.title")}>
            <Thead>
                <Tr>
                    <Th>{_("panels.drives.col.node")}</Th>
                    <Th>{_("panels.drives.col.model")}</Th>
                    <Th>{_("panels.drives.col.capacity")}</Th>
                    <Th>{_("panels.drives.col.smart")}</Th>
                    <Th>{_("panels.drives.col.arrays")}</Th>
                </Tr>
            </Thead>
            <Tbody>
                {report.disks.length > 0
                    ? report.disks.map(disk => <DiskRow disk={disk} arrays={report.arrays} key={disk.name} />)
                    : <EmptyRow columns={5} message={_("panels.drives.empty")} />}
            </Tbody>
        </Table>
    </Section>
);

const ArrayRow = ({ array }: { array: ArrayStatus }) => {
    const active = array.active_disks ?? array.members.length;
    const expected = array.raid_disks ?? array.members.length;
    const isInvalidRaid6 = array.level?.toLowerCase() === "raid6" && expected < 4;
    const tone: Tone = arrayNeedsAttention(array)
        ? { label: _("panels.arrays.attention"), tone: "warning" }
        : { label: _("common.ok"), tone: "good" };

    return (
        <Tr>
            <Td dataLabel={_("panels.arrays.col.device")}><strong className={MONO}>/dev/{array.name}</strong></Td>
            <Td dataLabel={_("common.raidLevel")} className={MONO}>{array.level?.toUpperCase() ?? _("common.unknown")}</Td>
            <Td dataLabel={_("panels.arrays.col.members")}>
                <span className={MONO}>{active}/{expected}</span>
                <MemberList
                    members={array.members}
                    memberStates={array.member_states}
                    emptyLabel={_("panels.arrays.noMemberInfo")}
                />
            </Td>
            <Td dataLabel={_("panels.arrays.col.sync")}>
                {array.sync
                    ? (
                        <>
                            <strong>{describeSyncAction(array.sync.action)}</strong>
                            {/* `formatSyncPercentEta` (model.ts) routes the ETA through
                                `formatDuration`, same as `formatSyncProgress` used one panel
                                below for bands -- this used to print raw minutes ("about
                                540.0 min") next to a correctly-formatted duration, and is now a
                                plain function `model.test.ts` asserts on directly instead of
                                inline JSX with no test surface. */}
                            <CellSub>{formatSyncPercentEta(array.sync)}</CellSub>
                        </>
                    )
                    : <Muted>{_("common.idle")}</Muted>}
            </Td>
            <Td dataLabel={_("panels.arrays.col.state")}>
                <Badge tone={tone} />
                <CellSub>
                    {[
                        describeArrayState(array.state),
                        array.read_only ? _("panels.arrays.readOnly") : null,
                        array.degraded ? _("panels.arrays.degraded") : null,
                        isInvalidRaid6 ? _("panels.arrays.invalidRaid6") : null,
                    ].filter(Boolean).join(" · ")}
                </CellSub>
            </Td>
        </Tr>
    );
};

export const ArraysPanel = ({ report }: { report: StatusReport }) => (
    <Section
        title={_("panels.arrays.title")}
        note={format(
            ngettext("panels.arrays.note.one", "panels.arrays.note.other", report.arrays.length),
            report.arrays.length,
        )}
    >
        <Table variant="compact" aria-label={_("panels.arrays.title")}>
            <Thead>
                <Tr>
                    <Th>{_("panels.arrays.col.device")}</Th>
                    <Th>{_("common.raidLevel")}</Th>
                    <Th>{_("panels.arrays.col.members")}</Th>
                    <Th>{_("panels.arrays.col.sync")}</Th>
                    <Th>{_("panels.arrays.col.state")}</Th>
                </Tr>
            </Thead>
            <Tbody>
                {report.arrays.length > 0
                    ? report.arrays.map(array => <ArrayRow array={array} key={array.name} />)
                    : <EmptyRow columns={5} message={_("panels.arrays.empty")} />}
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
        return _("panels.tolerance.unknownMode");
    if (remaining === null)
        return format(ngettext(
            "panels.tolerance.design.one",
            "panels.tolerance.design.other",
            nominal,
        ), nominal);
    if (remaining === nominal)
        return format(ngettext("panels.tolerance.nominal.one", "panels.tolerance.nominal.other", nominal), nominal);
    if (remaining >= 0)
        return format(_("panels.tolerance.remaining"), remaining, nominal);
    // Never clamp to 0 -- a band already beyond its tolerance must read as
    // beyond it, not as "no margin left" (which would understate the risk).
    return format(_("panels.tolerance.exceeded"), nominal);
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
            <Td dataLabel={_("panels.band.col.band")} className={MONO}>band{band.index}</Td>
            <Td dataLabel={_("common.raidLevel")} className={MONO}>{band.level.toUpperCase()}</Td>
            <Td dataLabel={_("panels.band.col.device")}>
                <span className={MONO}>/dev/{band.md_name}</span>
                <CellSub className={MONO}>{band.md_uuid ?? _("panels.band.uuidUnknown")}</CellSub>
            </Td>
            <Td dataLabel={_("panels.band.col.members")}>
                {band.members.length > 0
                    ? (
                        <>
                            <MemberList members={band.members} memberStates={band.member_states} />
                            {capacity && (
                                <CellSub>
                                    <span title={_("panels.band.sliceHint")}>
                                        {format(
                                            ngettext(
                                                "panels.band.slice.one",
                                                "panels.band.slice.other",
                                                capacity.memberCount,
                                            ),
                                            formatBytes(capacity.memberBytes),
                                            capacity.memberCount,
                                        )}
                                    </span>
                                </CellSub>
                            )}
                        </>
                    )
                    : <Muted>{_("panels.band.noLiveMembers")}</Muted>}
            </Td>
            <Td dataLabel={_("panels.band.col.capacity")} className={MONO}>
                {formatBytes(band.usable_bytes)} / {capacity ? formatBytes(capacity.rawBytes) : _("common.unknown")}
            </Td>
            <Td dataLabel={_("panels.band.col.syncScrub")}>
                {band.resize_pending && <Badge tone={{ label: _("common.resizePending"), tone: "warning" }} />}
                {/* `band.sync === null` covers BOTH "live array, nothing
                    syncing" and "no live mdadm array with this md_name at all"
                    -- see GroupBandStatus::sync's doc comment. Reading
                    `band.members.length === 0` (the same "no live array"
                    signal the member cell above already uses) first, exactly
                    like render.rs's render_band_detail_row/watch_band_row do
                    for this identical field, is what tells the two apart
                    instead of both silently reading as "Idle". Scrub
                    (below) is intentionally NOT gated the same way: it's
                    state.toml history, not a live-mdstat read, so it stays
                    meaningful even with no live array -- matches
                    render_band_detail_row, which doesn't gate scrub either. */}
                <CellSub>
                    {band.members.length === 0
                        ? _("common.noLiveArrayInfo")
                        : band.sync ? formatSyncProgress(band.sync) : _("common.idle")}
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
                <Split hasGutter isWrappable>
                    <SplitItem isFilled>
                        <Flex spaceItems={{ default: "spaceItemsSm" }} alignItems={{ default: "alignItemsCenter" }}>
                            <FlexItem><strong className={MONO}>{group.name}</strong></FlexItem>
                            <FlexItem><Chip>{modeLabel(group.mode)}</Chip></FlexItem>
                            {group.resize_pending && (
                                <FlexItem>
                                    <Badge tone={{ label: _("common.resizePending"), tone: "warning" }} />
                                </FlexItem>
                            )}
                            {scrubWarnings > 0 && (
                                <FlexItem>
                                    <Badge tone={{
                                        label: format(
                                            ngettext(
                                                "panels.groups.scrubWarn.one",
                                                "panels.groups.scrubWarn.other",
                                                scrubWarnings,
                                            ),
                                            scrubWarnings,
                                        ),
                                        tone: "warning",
                                    }}
                                    />
                                </FlexItem>
                            )}
                        </Flex>
                    </SplitItem>
                    <SplitItem className={MONO}>{formatBytes(group.usable_bytes)}</SplitItem>
                </Split>
            </CardTitle>
            <CardBody>
                {/* `orientation={{ md: "horizontal" }}` rather than
                    `isHorizontal`, and the two are NOT additive. `isHorizontal`
                    emits the unconditional `pf-m-horizontal`, which pins the
                    list to a term column plus a value column at every width.
                    Measured on the pre-change bundle at 390px: a 111px term
                    column left the value column 135px, for values like a
                    36-character `fs_uuid`. It is 262px after this change.
                    `orientation` has no `default` key (its keys are
                    sm/md/lg/xl/2xl), so `{ md: "horizontal" }` emits
                    `pf-m-horizontal-on-md`: horizontal from 768px up, and the
                    component's own vertical base layout below it. Keeping
                    `isHorizontal` alongside would re-pin it and defeat the
                    change, so it is removed rather than supplemented.
                    768px is deliberate: it is the same width the tables
                    already collapse at (`gridBreakPoint: grid-md`), so the
                    whole page changes shape once rather than twice. */}
                <DescriptionList orientation={{ md: "horizontal" }} isCompact className="pf-v6-u-mb-md">
                    <DescriptionListGroup>
                        <DescriptionListTerm>{_("panels.group.mountPoint")}</DescriptionListTerm>
                        <DescriptionListDescription className={MONO}>{group.mount_point}</DescriptionListDescription>
                    </DescriptionListGroup>
                    {/* No layout version here: it is an internal on-disk
                        revision number that means nothing to whoever is
                        reading this card. */}
                    <DescriptionListGroup>
                        <DescriptionListTerm>{_("panels.group.fsUuid")}</DescriptionListTerm>
                        <DescriptionListDescription className={MONO}>
                            {group.fs_uuid ?? _("common.unknown")}
                        </DescriptionListDescription>
                    </DescriptionListGroup>
                    <DescriptionListGroup>
                        <DescriptionListTerm>{_("panels.group.disks")}</DescriptionListTerm>
                        <DescriptionListDescription>
                            {group.disks.length > 0
                                ? (
                                    <Flex spaceItems={{ default: "spaceItemsXs" }}>
                                        {/* `title` because PatternFly's Label
                                            truncates its own text with an
                                            ellipsis rather than wrapping, and a
                                            `by-id` name is long enough to lose
                                            its distinguishing tail at a phone
                                            width: two disks of the same model
                                            differ only in the serial at the
                                            end. The full value stays reachable
                                            on hover and to a screen reader. */}
                                        {group.disks.map(id => (
                                            <FlexItem key={id}><Chip title={id}>{id}</Chip></FlexItem>
                                        ))}
                                    </Flex>
                                )
                                : <Muted>{_("common.none")}</Muted>}
                        </DescriptionListDescription>
                    </DescriptionListGroup>
                </DescriptionList>
                <Table variant="compact" aria-label={format(_("panels.group.bandTableLabel"), group.name)}>
                    <Thead>
                        <Tr>
                            <Th>{_("panels.band.col.band")}</Th>
                            <Th>{_("common.raidLevel")}</Th>
                            <Th>{_("panels.band.col.device")}</Th>
                            <Th>{_("panels.band.col.members")}</Th>
                            <Th>{_("panels.band.col.capacity")}</Th>
                            <Th>{_("panels.band.col.syncScrub")}</Th>
                        </Tr>
                    </Thead>
                    <Tbody>
                        {group.bands.length > 0
                            ? group.bands.map(band => <BandRow band={band} arrays={arrays} key={band.index} />)
                            : <EmptyRow columns={6} message={_("panels.group.noBands")} />}
                    </Tbody>
                </Table>
            </CardBody>
        </Card>
    );
};

export const GroupsPanel = ({ report }: { report: StatusReport }) => (
    <Section
        title={_("panels.groups.title")}
        note={[
            format(ngettext("panels.groups.note.one", "panels.groups.note.other", report.groups.length), report.groups.length),
            report.groups.some(g => g.resize_pending) ? _("panels.groups.expansionPending") : null,
        ].filter(Boolean).join(" · ")}
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
                            {_("panels.groups.empty")}
                        </Caveat>
                    </StackItem>
                )}
        </Stack>
    </Section>
);

// --- Capacity overview (mockup: top metrics-grid + allocation bar) ---------

// A function rather than the module-scope constant this used to be. po.js runs
// before the bundle (see index.html), so a constant would in fact translate
// correctly today -- but it would be the one string table in this package whose
// correctness depends on script order rather than on when it is read.
const segmentLabel = (kind: string): string => ({
    used: _("panels.segment.used"),
    free: _("panels.segment.free"),
    unknown: _("panels.segment.unknown"),
    parity: _("panels.segment.parity"),
    unassigned: _("panels.segment.unassigned"),
    system: _("common.systemDisk"),
}[kind] ?? kind);

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
        ? _("panels.capacity.usedUnknown.fsDfFailed")
        : fsDf.groups.length === 0
            ? _("panels.capacity.usedUnknown.noGroups")
            : _("panels.capacity.usedUnknown.partial");

    return (
        <>
            <Gallery hasGutter minWidths={{ default: "220px" }} aria-label={_("panels.capacity.overviewLabel")}>
                <MetricCard
                    label={_("panels.capacity.usable")}
                    value={formatBytes(allocation.usableBytes)}
                    sub={format(_("panels.capacity.physicalSub"), formatBytes(allocation.rawDiskBytes))}
                />
                <MetricCard
                    label={_("panels.capacity.used")}
                    value={usage.usedBytes === null ? _("common.notMeasurable") : formatBytes(usage.usedBytes)}
                    sub={usage.usedBytes === null ? usedUnknownReason : _("panels.capacity.usedMeasured")}
                />
                <MetricCard
                    label={_("panels.capacity.free")}
                    value={usage.freeBytes === null ? _("common.notMeasurable") : formatBytes(usage.freeBytes)}
                    sub={usage.freeBytes === null
                        ? _("panels.capacity.freeUnknown")
                        : _("panels.capacity.freeAvailable")}
                />
                <MetricCard
                    label={_("panels.capacity.protection")}
                    value={allocation.parityBytes === null
                        ? _("common.notMeasurable")
                        : format(_("panels.capacity.parityValue"), formatBytes(allocation.parityBytes))}
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
                                {allocation.parityBytesPartial && (
                                    <StackItem>{_("panels.capacity.parityPartial")}</StackItem>
                                )}
                            </Stack>
                        )
                        : _("panels.capacity.noGroups")}
                />
            </Gallery>

            <Card component="div" aria-label={_("panels.allocation.title")}>
                <CardTitle>
                    <Split hasGutter isWrappable>
                        <SplitItem isFilled>{_("panels.allocation.title")}</SplitItem>
                        <SplitItem className={MONO}>
                            {report.groups.map(g => modeLabel(g.mode)).join(", ") || _("panels.allocation.noGroups")}
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
                                                title={`${segmentLabel(segment.kind)} (${formatBytes(segment.bytes)})`}
                                                measureLocation={ProgressMeasureLocation.outside}
                                                variant={segment.kind === "used" ? "success" : undefined}
                                                aria-label={segmentLabel(segment.kind)}
                                            />
                                        </StackItem>
                                    ))}
                                </Stack>
                                {allocation.systemDiskBytes !== null && allocation.systemDiskBytes > 0 && (
                                    <Caveat>
                                        {_("panels.allocation.systemDiskNote")}
                                    </Caveat>
                                )}
                            </>
                        )
                        : <Caveat>{_("panels.allocation.empty")}</Caveat>}
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

const TechSpecCard = ({ group, statePath }: { group: GroupStatus; statePath: string | null }) => {
    const unknown = _("common.unknown");
    return (
        <DescriptionList orientation={{ md: "horizontal" }} isCompact>
            <KvItem label={_("panels.tech.groupName")} value={group.name} />
            <KvItem label={_("panels.tech.vg")} value={group.vg_name ?? unknown} />
            <KvItem
                label={_("panels.tech.lv")}
                value={`${group.lv_name ?? unknown} → ${group.mount_point}`}
            />
            <KvItem label={_("panels.group.fsUuid")} value={group.fs_uuid ?? unknown} />
            <KvItem label={_("panels.tech.compression")} value={group.compression ?? unknown} />
            {statePath === null
                ? (
                    <KvItem
                        label={_("panels.tech.statePath")}
                        value={unknown}
                        // The status report carries this path, so the hint only
                        // fires when the backend genuinely does not know it
                        // (an older CLI build).
                        hint={_("panels.tech.statePathHint")}
                    />
                )
                : (
                    <KvItem
                        label={_("panels.tech.statePath")}
                        value={statePath}
                    />
                )}
        </DescriptionList>
    );
};

export const TechSpecPanel = ({ report }: { report: StatusReport }) => (
    <Section title={_("panels.tech.title")} note={_("panels.tech.note")} defaultExpanded={false}>
        <Stack hasGutter>
            {report.groups.length > 0
                ? report.groups.map(group => (
                    <StackItem key={group.name}>
                        <Title headingLevel="h3" size="md" className={MONO}>{group.name}</Title>
                        <TechSpecCard group={group} statePath={report.state_path} />
                    </StackItem>
                ))
                : <StackItem><Caveat>{_("panels.tech.empty")}</Caveat></StackItem>}
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
        <Section
            title={_("panels.method.title")}
            note={_("panels.method.note")}
            defaultExpanded={false}
        >
            <DescriptionList orientation={{ md: "horizontal" }} isCompact className="pf-v6-u-mb-md">
                <DescriptionListGroup>
                    <DescriptionListTerm>{_("common.rawCapacity")}</DescriptionListTerm>
                    <DescriptionListDescription className={MONO}>
                        {formatBytes(summary.rawBytes)}
                    </DescriptionListDescription>
                </DescriptionListGroup>
                <DescriptionListGroup>
                    <DescriptionListTerm>{_("panels.method.usableCapacity")}</DescriptionListTerm>
                    <DescriptionListDescription className={MONO}>
                        {formatBytes(allocation.usableBytes)}
                    </DescriptionListDescription>
                </DescriptionListGroup>
                <DescriptionListGroup>
                    <DescriptionListTerm>{_("panels.method.parityCapacity")}</DescriptionListTerm>
                    <DescriptionListDescription className={MONO}>
                        {formatBytes(allocation.parityBytes)}
                    </DescriptionListDescription>
                </DescriptionListGroup>
            </DescriptionList>
            {/* One paragraph per figure rather than one msgid with `<strong>`
                markup inside it: a translator gets whole sentences, and none
                of them has to carry HTML through the catalogue. */}
            <Content component="p">
                <strong>{_("panels.method.usableTerm")}</strong>{" "}
                {_("panels.method.usableBody")}
            </Content>
            <Content component="p">
                <strong>{_("panels.method.parityTerm")}</strong>{" "}
                {_("panels.method.parityBody")}
            </Content>
            <Content component="p">
                <strong>{_("panels.capacity.used")}</strong>{" "}
                {_("panels.method.usedBody")}
            </Content>
            <Content component="p">
                <strong>{_("common.rawCapacity")}</strong>{" "}
                {_("panels.method.rawBody")}
            </Content>
        </Section>
    );
};
