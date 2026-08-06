/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

import React, { useCallback, useEffect, useRef, useState } from "react";

import {
    Alert,
    Bullseye,
    Button,
    Card,
    CardBody,
    CardTitle,
    Content,
    EmptyState,
    EmptyStateBody,
    Flex,
    FlexItem,
    Gallery,
    Label,
    Page,
    PageSection,
    Spinner,
    Split,
    SplitItem,
    Title,
} from "@patternfly/react-core";

import { OperationsPanel } from "./actionsDialogs.js";
import { SPAWN_OPTIONS, type Spawn } from "./actions.js";
import cockpit, { type CockpitError } from "./cockpit.js";
import { CreateGroupWizard } from "./createGroupWizard.js";
import { _, format, ngettext } from "./i18n.js";
import {
    type FsDfReport,
    type StatusReport,
    describeBackgroundActivity,
    formatBytes,
    parseFsDfOutput,
    parseStatusOutput,
    summarizeStatus,
} from "./model.js";
import {
    ArraysPanel,
    CapacityMethodologyPanel,
    CapacityOverviewPanel,
    DrivesPanel,
    GroupsPanel,
    TechSpecPanel,
    healthTone,
} from "./panels.js";
import { Badge, Caveat, MONO, MetricCard } from "./ui.js";

type ErrorHintKind = "permission" | "installation";

type LoadState =
    | { kind: "loading" }
    | { kind: "ready"; report: StatusReport; fsDf: FsDfReport | null; refreshedAt: Date }
    | { kind: "error"; message: string; hint: ErrorHintKind };

const errorMessage = (error: unknown): string => {
    if (typeof error === "string" && error.trim())
        return error;
    if (typeof error === "object" && error !== null) {
        const cockpitError = error as CockpitError;
        if (cockpitError.message?.trim())
            return cockpitError.message;
        if (cockpitError.problem)
            return format(_("app.error.connection"), cockpitError.problem);
    }
    return _("app.error.loadFailed");
};

/**
 * The error panel's hint always blamed a missing `shr-rs`
 * install/PATH, even when the real cause is that Cockpit rejected the spawn
 * for lacking admin privileges. Reproduced in a real browser: logged into
 * Cockpit as `dev` in limited-access mode (admin not activated), the
 * `superuser: "require"` spawn rejects with `problem: "access-denied"` and
 * `exit_status: null` -- captured live from the plugin frame, not read off
 * docs. Cockpit does NOT auto-prompt for privilege elevation on its own
 * here, so the error panel is the only thing the user sees, and the old hint
 * sent them to diagnose an install problem they don't have.
 *
 * Classifies on `CockpitError.problem` (see cockpit.ts) -- a structured
 * field Cockpit itself sets, not a substring match against the `message`
 * text. That distinction is not theoretical: the same rejection above was
 * observed rendering as "Not permitted to perform this action." on page load
 * and as its Korean translation moments later in the
 * same frame (Cockpit's translations load asynchronously), so any hint keyed
 * off the message string would fire inconsistently on one machine.
 * `"access-denied"` is Cockpit's own code
 * for "this session can't become root" (superuser required, not active or
 * not permitted); `"not-found"` is its code for "the executable could not
 * be located" (the actual install/PATH failure -- see
 * https://cockpit-project.org/guide/latest/cockpit-spawn). Any other or
 * missing `problem` (a transport error, a plain JS `Error`, ...) falls back
 * to the install/PATH hint, matching this function's older universal
 * default for every failure shape it hasn't been taught to distinguish yet.
 */
export const errorHintKind = (error: unknown): ErrorHintKind => {
    const problem = typeof error === "object" && error !== null
        ? (error as CockpitError).problem
        : null;
    return problem === "access-denied" ? "permission" : "installation";
};

const Dashboard = (
    { report, fsDf, refreshedAt, onChanged }: {
        report: StatusReport;
        fsDf: FsDfReport | null;
        refreshedAt: Date;
        onChanged: () => void;
    },
) => {
    const summary = summarizeStatus(report);
    const health = healthTone(report.health);
    // Derived from `report.arrays` (live mdstat-sourced `sync`), never
    // from a group's `scrub_in_progress`/resize flags -- see
    // `describeBackgroundActivity`'s doc comment in model.ts. `report.groups`
    // is only consulted to tell "no bands configured" apart from
    // "bands expected but no live array answered" -- it never substitutes for
    // the live `arrays` read.
    const activity = describeBackgroundActivity(report.arrays, report.groups);
    const rawCapacitySub = [
        summary.unknownSizeDisks > 0
            ? format(
                ngettext("app.raw.unknownSize.one", "app.raw.unknownSize.other", summary.unknownSizeDisks),
                summary.unknownSizeDisks,
            )
            : _("app.raw.allSizesKnown"),
        summary.systemDisks > 0
            ? format(
                ngettext(
                    "app.raw.includesSystem.one",
                    "app.raw.includesSystem.other",
                    summary.systemDisks,
                ),
                summary.systemDisks,
            )
            : null,
    ].filter(Boolean).join(" · ");

    return (
        <>
            <Gallery hasGutter minWidths={{ default: "220px" }} aria-label={_("app.summaryLabel")}>
                <MetricCard
                    label={_("app.metric.background")}
                    value={<Badge tone={{ label: activity.headline, tone: activity.tone }} />}
                    sub={activity.detail}
                />
                <MetricCard
                    label={_("app.metric.overall")}
                    value={<Badge tone={health} />}
                    sub={_("app.metric.overallSub")}
                />
                <MetricCard
                    label={_("app.metric.drives")}
                    value={format(ngettext("common.driveCount.one", "common.driveCount.other", report.disks.length), report.disks.length)}
                    sub={format(
                        ngettext("common.smartWarningCount.one", "common.smartWarningCount.other", summary.warningDisks),
                        summary.warningDisks,
                    )}
                />
                <MetricCard
                    label={_("common.rawCapacity")}
                    value={formatBytes(summary.rawBytes)}
                    sub={rawCapacitySub}
                />
                <MetricCard
                    label={_("app.metric.members")}
                    value={`${summary.activeMembers}/${summary.expectedMembers}`}
                    sub={format(
                        ngettext("app.metric.arraysNeedAttention.one", "app.metric.arraysNeedAttention.other", summary.warningArrays),
                        summary.warningArrays,
                    )}
                />
            </Gallery>

            <CapacityOverviewPanel report={report} fsDf={fsDf} />

            <Card component="div" aria-label={_("app.physical.title")}>
                <CardTitle>
                    {/* `isWrappable` on every card-title Split in this plugin.
                        `.pf-v6-l-split` declares no `flex-wrap` at all, so it
                        takes the initial `nowrap` and its items can only
                        compress, never reflow; `pf-m-wrap` is the modifier that
                        changes that, and `isWrappable` is what emits it.
                        (Measured on the pre-change bundle: `nowrap` at both
                        widths. Unlike the action rows, no Split item was
                        actually pushed past the viewport at 390px, so this
                        corrects the mechanism, not a clipping defect.)
                        `isFilled` on the left item is kept: on a wrapped line
                        it simply takes the full width, which is the stacked
                        result we want. */}
                    <Split hasGutter isWrappable>
                        <SplitItem isFilled>
                            {_("app.physical.title")}
                            <Content component="small" className="pf-v6-u-text-color-subtle">
                                {_("app.physical.sub")}
                            </Content>
                        </SplitItem>
                        <SplitItem className={MONO}>{formatBytes(summary.rawBytes)}</SplitItem>
                    </Split>
                </CardTitle>
                <CardBody>
                    {/* The colour key the old `.legend-dot` spans drew by hand.
                        `Label` carries PatternFly's own chart-colour tokens, so
                        these stay legible in both themes without this file
                        naming a single hex value. */}
                    <Flex spaceItems={{ default: "spaceItemsSm" }} className="pf-v6-u-mb-md">
                        <FlexItem>
                            <Label isCompact color="blue">
                                {format(
                                    ngettext("app.legend.linked.one", "app.legend.linked.other", summary.linkedDisks),
                                    summary.linkedDisks,
                                )}
                            </Label>
                        </FlexItem>
                        <FlexItem>
                            <Label isCompact color="grey">
                                {format(
                                    ngettext("app.legend.unlinked.one", "app.legend.unlinked.other", summary.unlinkedDisks),
                                    summary.unlinkedDisks,
                                )}
                            </Label>
                        </FlexItem>
                        {summary.systemDisks > 0 && (
                            <FlexItem>
                                <Label isCompact color="orange">
                                    {format(
                                        ngettext(
                                            "app.legend.system.one",
                                            "app.legend.system.other",
                                            summary.systemDisks,
                                        ),
                                        summary.systemDisks,
                                    )}
                                </Label>
                            </FlexItem>
                        )}
                    </Flex>
                    <Caveat>
                        {_("app.physical.caveat")}
                        <span title={_("app.physical.caveatHint")}>
                            {" "}ⓘ
                        </span>
                    </Caveat>
                </CardBody>
            </Card>

            <OperationsPanel groups={report.groups} disks={report.disks} onChanged={onChanged} />

            <GroupsPanel report={report} />
            <DrivesPanel report={report} warningDisks={summary.warningDisks} />
            <ArraysPanel report={report} />
            <TechSpecPanel report={report} />
            <CapacityMethodologyPanel report={report} />

            <Content component="small" className="pf-v6-u-text-color-subtle">
                {/* No explicit locale tag: `toLocaleString()` follows the
                    browser's own language, which is also what decided which
                    po.js Cockpit served us. Pinning "ko-KR" here printed a
                    Korean date on an English page. */}
                {format(_("app.lastRefreshed"), refreshedAt.toLocaleString())}
            </Content>
        </>
    );
};

/**
 * Fetches `status --json` and (best-effort) `fs df --json` -- the exact pair
 * `refresh()` below needs on every load/refresh. Pulled out as a
 * `spawn`-injected function (same dependency-injection discipline as
 * `actions.ts`/`createGroup.ts`: "spawn is injected, no DOM/`cockpit` global
 * reference") so it can be pinned by a test without mounting React or
 * touching `window.cockpit` -- see `app.test.ts`.
 *
 * `fs df` is supplementary (the capacity panel degrades gracefully when it's
 * missing -- see `CapacityOverviewPanel`): a failure here (older shr-rs
 * without the `fs` subcommand, transport error, ...) must not take down the
 * whole dashboard the way a `status` failure does, so it's caught locally
 * rather than left to reject.
 *
 * Both calls use `actions.ts`'s own `SPAWN_OPTIONS` (`superuser:
 * "require"`), not `"try"` -- this used to be the one spawn call in the
 * project not covered by that rule. Reproduced on a real guest: with
 * `/var/lib/shr-rs/state.toml` owned `root:root 0600` (the normal
 * post-install state), `"try"` silently ran `status --json` unprivileged
 * instead of prompting for admin access, and the dashboard showed nothing
 * but "Permission denied (os error 13)". `"require"` makes Cockpit ask for
 * privilege up front, the same as every other action in this project.
 */
export const fetchDashboardState = async (spawn: Spawn): Promise<{ output: string; fsDf: FsDfReport | null }> => {
    const statusPromise = spawn(["shr-rs", "status", "--json"], SPAWN_OPTIONS);
    const fsDfPromise = spawn(["shr-rs", "fs", "df", "--json"], SPAWN_OPTIONS)
            .then(parseFsDfOutput)
            .catch(() => null);
    const [output, fsDf] = await Promise.all([statusPromise, fsDfPromise]);
    return { output, fsDf };
};

export const Application = () => {
    const [state, setState] = useState<LoadState>({ kind: "loading" });
    const [showWizard, setShowWizard] = useState(false);
    const requestGeneration = useRef(0);

    const refresh = useCallback(() => {
        const generation = ++requestGeneration.current;
        setState({ kind: "loading" });
        fetchDashboardState(cockpit.spawn)
                .then(({ output, fsDf }) => {
                    if (generation === requestGeneration.current) {
                        setState({
                            kind: "ready",
                            report: parseStatusOutput(output),
                            fsDf,
                            refreshedAt: new Date(),
                        });
                    }
                })
                .catch(error => {
                    if (generation === requestGeneration.current)
                        setState({ kind: "error", message: errorMessage(error), hint: errorHintKind(error) });
                });
    }, []);

    useEffect(() => {
        refresh();
    }, [refresh]);

    const health = state.kind === "ready"
        ? healthTone(state.report.health)
        : { label: state.kind === "error" ? _("app.badge.connectionError") : _("app.badge.checking"), tone: "neutral" as const };

    return (
        // `pf-m-no-sidebar` is PatternFly's own modifier, and the same one
        // Cockpit puts on its pages (`/system` renders
        // `pf-v6-c-page pf-m-no-sidebar`). It is not optional once the
        // masthead below is gone: page.css grants the main container the
        // full-width grid area through
        // `.pf-v6-c-page.pf-m-no-sidebar, .pf-v6-c-masthead + .pf-v6-c-page__main-container, ...`
        // -- so the masthead had been silently supplying it via that sibling
        // selector. Without either, `.pf-v6-c-page`'s grid keeps its
        // `"sidebar main"` columns and reserves 290px for a sidebar this
        // plugin never renders, indenting every page by that much.
        <Page className="pf-m-no-sidebar">
            {/*
              * No <Masthead>. A Cockpit plugin is an iframe inside the
              * shell, and the shell already draws the masthead above it --
              * `/system` and `/system/services` both render a bare <Page>
              * whose main holds only page sections. Drawing our own was not
              * just a second header: PatternFly's Page puts children in
              * `.pf-v6-c-page__main-container`, a grid item carrying
              * `z-index: 100`, and a grid item with a z-index forms a
              * stacking context even while `position: static`. That traps
              * every descendant -- including a dialog's fixed `Backdrop`
              * (z 400) and `ModalBox` (z 500) -- at an effective 100, below
              * the masthead's 300. Every dialog header in this plugin
              * rendered *under* the masthead, which made each dialog's close
              * button unclickable. So the title, subtitle, health badge and
              * both actions live in a leading page section instead, the same
              * shape Cockpit's own overview header uses (a title block left,
              * actions right).
              */}
            <PageSection hasBodyWrapper={false}>
                {/* No `flexWrap` here or on the action group below: PatternFly's
                    Flex already defaults to `--pf-v6-l-flex--FlexWrap: wrap`,
                    so both rows reflow at a phone width on their own and
                    `flexWrap={{ default: "wrap" }}` would only re-declare the
                    value already in effect. `Split` is the layout that does
                    NOT wrap by default; see the card titles below. */}
                <Flex
                    justifyContent={{ default: "justifyContentSpaceBetween" }}
                    alignItems={{ default: "alignItemsCenter" }}
                    spaceItems={{ default: "spaceItemsMd" }}
                >
                    <FlexItem>
                        {/* No `size`: PatternFly then maps the heading level to
                            its own scale (`pf-m-h1`, 24px), instead of us
                            picking a t-shirt size by eye. That is also what
                            stock Cockpit's page title measures -- though it
                            gets there through its own `ct-overview-header`
                            CSS on a bare <h1>, which is the kind of hand-written
                            rule this migration exists to delete. */}
                        <Title headingLevel="h1">SHR-RS RAID Manager</Title>
                        <Content component="small">
                            {_("app.subtitle")}
                        </Content>
                    </FlexItem>
                    <FlexItem>
                        <Flex
                            alignItems={{ default: "alignItemsCenter" }}
                            spaceItems={{ default: "spaceItemsSm" }}
                        >
                            <FlexItem><Badge tone={health} /></FlexItem>
                            <FlexItem>
                                <Button
                                    variant="primary"
                                    onClick={() => setShowWizard(true)}
                                    isDisabled={state.kind !== "ready"}
                                >
                                    {_("app.action.createGroup")}
                                </Button>
                            </FlexItem>
                            <FlexItem>
                                <Button
                                    variant="secondary"
                                    onClick={refresh}
                                    isDisabled={state.kind === "loading"}
                                >
                                    {state.kind === "loading" ? _("app.action.loading") : _("app.action.refresh")}
                                </Button>
                            </FlexItem>
                        </Flex>
                    </FlexItem>
                </Flex>
            </PageSection>

            <PageSection hasBodyWrapper={false}>
                {state.kind === "loading" && (
                    <Bullseye>
                        <EmptyState
                            titleText={_("app.loading.title")}
                            headingLevel="h2"
                            icon={Spinner}
                            aria-live="polite"
                        >
                            <EmptyStateBody>
                                {_("app.loading.body")}
                            </EmptyStateBody>
                        </EmptyState>
                    </Bullseye>
                )}

                {state.kind === "error" && (
                    <Alert
                        variant="danger"
                        isInline
                        title={_("app.error.title")}
                        actionLinks={<Button variant="link" isInline onClick={refresh}>{_("app.error.retry")}</Button>}
                    >
                        <p>{state.message}</p>
                        <p>
                            {state.hint === "permission"
                                ? _("app.error.hint.permission")
                                : _("app.error.hint.installation")}
                        </p>
                    </Alert>
                )}

                {state.kind === "ready" && (
                    <Flex direction={{ default: "column" }} spaceItems={{ default: "spaceItemsLg" }}>
                        <Dashboard
                            report={state.report}
                            fsDf={state.fsDf}
                            refreshedAt={state.refreshedAt}
                            onChanged={refresh}
                        />
                    </Flex>
                )}
            </PageSection>

            {showWizard && state.kind === "ready" && (
                <CreateGroupWizard
                    disks={state.report.disks}
                    existingGroupNames={state.report.groups.map(group => group.name)}
                    // `GroupStatus.vg_name` is required/always-present on a real
                    // payload (`parseGroup` throws if it's missing) -- the `?? ""`
                    // fallback here only matters for the type's optional marker (kept
                    // optional for OTHER files' older fixtures, see model.ts), and an
                    // empty string can never collide with a real (non-empty) VG name.
                    existingGroupVgNames={state.report.groups.map(group => ({ name: group.name, vg_name: group.vg_name ?? "" }))}
                    onClose={() => setShowWizard(false)}
                    onCreated={() => {
                        setShowWizard(false);
                        refresh();
                    }}
                />
            )}
        </Page>
    );
};
