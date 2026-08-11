/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * The React shell for Cockpit's operational actions -- scrub, expand, disk
 * replace, snapshot create, recompress, schedule install.
 * Deliberately thin, same split as `createGroupWizard.tsx`: every safety-
 * relevant decision (what to spawn, when execution is allowed, how a
 * backend failure is interpreted) lives in `actions.ts` and is covered by
 * `actions.test.ts`. This file is UI wiring only.
 *
 * `OperationsPanel` is the single mountable export -- see its own doc
 * comment below for exactly how to mount it.
 *
 * PatternFly conversion note -- read this before "fixing" a raw <button>/
 * <input>/<select> below. Every interactive control in this file (all
 * <button>s, the two <input type="checkbox">s, every <input type="text">,
 * both <select>s) is intentionally a raw native element decorated with
 * PatternFly's own literal CSS class tokens (`pf-v6-c-button pf-m-primary`,
 * `pf-v6-c-check`, `pf-v6-c-form-control`, ...), not PatternFly's own React
 * components. `createGroupWizard.tsx` uses real PF `Button`/`Checkbox`/
 * `TextInput` -- that's fine there because it has no test file. This file
 * has one (`actionsDialogs.test.ts`), and four concrete mechanisms in
 * PatternFly 6's own component code are incompatible with how that test
 * file drives and asserts on this tree:
 *
 *   1. `Button` wraps text children in an inner
 *      `<span class="pf-v6-c-button__text">` (see node_modules/@patternfly/
 *      react-core/dist/js/components/Button/Button.js). The test's
 *      `findButtonByText` helper does `buttons.find(b => b.props.children
 *      === text)` -- strict equality against the rendered button's own
 *      `children` prop, which a Fragment-wrapped span fails.
 *   2. The same file renders `"aria-disabled": isAriaDisabled ||
 *      (!isButtonElement && isDisabled)` -- for a `component="button"`
 *      element this is just `isAriaDisabled`, which defaults to `false` but
 *      is still emitted (aria-* attributes are never omitted for a `false`
 *      value). `Modal`'s two tests assert the close button carries a bare
 *      `disabled=""` when busy and *no* substring matching `/disabled/` at
 *      all when idle; the ever-present `aria-disabled="false"` breaks the
 *      idle assertion regardless of which prop is used to disable it.
 *   3. `Checkbox`/`TextInput` read `event.currentTarget.{checked,value}`
 *      inside their own `onChange` handler, but this file's
 *      interactive tests (DestroyDialog's zero-superblocks checkbox and its
 *      typed-confirm input) drive `onChange` with hand-built fake events
 *      shaped `{ target: { checked | value } }` and no `currentTarget` --
 *      both would throw `TypeError: Cannot read properties of undefined`.
 *   4. `Alert` (see .../components/Alert/Alert.js:64) runs its own
 *      `useEffect` that unconditionally calls
 *      `document.addEventListener('focus', ...)` with no `typeof document
 *      !== "undefined"` guard. Harmless under `renderToStaticMarkup` (SSR
 *      never commits, so no effect ever fires), but a hard `ReferenceError:
 *      document is not defined` the instant `react-test-renderer`'s `act()`
 *      flushes passive effects -- exactly what this file's earlier interactive
 *      tests (Reconcile/Schedule/Destroy) do. Confirmed by reproduction, not
 *      just by reading the source.
 *
 *      Unlike 1-3, this one is a genuine PatternFly component used as-is --
 *      `Alert` renders its own icon and a screen-reader-only "Danger alert:"
 *      prefix that a hand-rolled substitute would either have to duplicate
 *      (defeating the point of this migration) or silently drop (a real
 *      a11y/UI regression, worst on the irreversible-action confirmations
 *      where the icon is a primary safety affordance). So the fix lives in
 *      the test file, not here: `actionsDialogs.test.ts` stubs
 *      `globalThis.document` with no-op `addEventListener`/
 *      `removeEventListener` and `activeElement: null`, which is exactly
 *      enough for this effect to register and unregister without ever
 *      dereferencing a real DOM. `Alert` itself is used unmodified below.
 *
 * None of this is about visual styling -- the same PatternFly CSS still
 * applies either way. `index.tsx` only imports `patternfly-base.css` and
 * `patternfly-addons.css` (neither carries component rules), but every
 * PatternFly import anywhere in `src/` is a barrel import (`from
 * "@patternfly/react-core"`), and both `@patternfly/react-core` and
 * `@patternfly/react-styles` declare `sideEffects` as an allowlist array
 * rather than `false` -- esbuild cannot tree-shake away the per-component
 * `import './button.css'` etc. those barrels pull in, so the full component
 * CSS tree (`pf-v6-c-button`, `pf-v6-c-check`, `pf-v6-c-form-control`,
 * `pf-v6-c-modal-box`, `pf-v6-c-alert`, ...) ships in the built stylesheet
 * regardless of whether any file actually uses the matching React
 * component. The raw elements below use those literal class names
 * directly; only non-interactive layout/structure (`Modal`'s shell,
 * `FormGroup`, `ExpandableSection`, `Card`, `Alert`, ...) uses real
 * PatternFly React components.
 */

import React, { useEffect, useMemo, useRef, useState } from "react";

import {
    ActionList,
    ActionListItem,
    Alert,
    Backdrop,
    Bullseye,
    Card,
    CardBody,
    CardExpandableContent,
    CardHeader,
    CardTitle,
    CodeBlock,
    CodeBlockCode,
    DescriptionList,
    DescriptionListDescription,
    DescriptionListGroup,
    DescriptionListTerm,
    ExpandableSection,
    FormGroup,
    HelperText,
    HelperTextItem,
    List,
    ListItem,
    ModalBody,
    ModalHeader,
    Spinner,
    Split,
    SplitItem,
    Stack,
    StackItem,
} from "@patternfly/react-core";
import TimesIcon from "@patternfly/react-icons/dist/esm/icons/times-icon";

import cockpit from "./cockpit.ts";
import {
    ExpandController,
    SimpleActionController,
    TypedConfirmController,
    buildReplaceInput,
    destroyArgs,
    filterExpandCandidates,
    filterReplacementCandidates,
    groupMemberDisks,
    hasStableId,
    isValidReplacement,
    parseDestroyResult,
    parseScrubStatus,
    parseTextResult,
    reconcileArgs,
    recompressArgs,
    replaceArgs,
    scheduleInstallArgs,
    scrubCancelArgs,
    scrubSpeedArgs,
    scrubStartArgs,
    scrubStatusArgs,
    snapshotCreateArgs,
    spawnErrorMessage,
    type DestroyInput,
    type DestroyResult,
    type ExpandInput,
    type ExpandState,
    type RecompressInput,
    type ReplaceInput,
    type ReshapePriority,
    type ScrubStatusReport,
    type SimpleActionState,
    type SnapshotInput,
    type TypedConfirmState,
    type WritePreflight,
} from "./actions.ts";
import { _, format, ngettext } from "./i18n.ts";
import { formatBytes, type DiskStatus, type GroupStatus } from "./model.ts";
import { ACTION_ROW, Badge, Caveat, FIELDSET_SHRINK, MONO, Muted, TITLE_WRAP } from "./ui.js";

// --- shared UI bits ----------------------------------------------------------

// Shown mid-operation, both as the × button's tooltip and as the reason
// text a dialog can show alongside its own now-disabled cancel button -- a
// disabled control with no explanation reads as a bug, not a safeguard.
const inFlightReason = () => _("dialogs.inFlightReason");

// `closeDisabled`: a multi-hour reshape/rebuild has no surface other
// than this dialog for its eventual success/error -- closing mid-operation
// (via × or a cancel/close button) discards that outcome even though the
// backend command keeps running. Every caller passes its own `busy` state
// (true exactly while its in-flight spawn is awaited), not the step-machine
// state -- see ExpandDialog/ReplaceDialog's controller, whose `state.step`
// only reaches "executing" after the awaited spawn already resolved, i.e.
// too late to gate anything on.
// Exported so `actionsDialogs.test.ts` can render it directly and
// assert the close button is actually `disabled` in the output, not just
// that a prop was threaded through -- the six dialogs below never render
// their own close affordance, they all delegate to this one.
//
// Hand-built rather than PatternFly's own `Modal`/`ModalBox` components:
// real `Modal` renders `null` under `react-dom/server` (its `render()`
// bails out whenever `!canUseDOM`, since it always tries to
// `ReactDOM.createPortal` into `document.body`, which doesn't exist under
// SSR) -- this file's tests use `renderToStaticMarkup`, so a portal-based
// Modal would make every dialog test see an empty string. `ModalBox`/
// `ModalBoxCloseButton` aren't even importable (only `Modal`, `ModalHeader`,
// `ModalBody`, `ModalFooter` are re-exported from the package root). The
// markup below reproduces `ModalBox`'s own rendered shape (`Backdrop` >
// `Bullseye` > a `div.pf-v6-c-modal-box` containing a header, a close
// button, and a body) using its own literal class names, with the close
// button itself a raw `<button>` for the reasons in this file's header
// comment.
//
// `pf-m-md` and `pf-m-align-top` are not decoration -- they are what
// stock Cockpit's own dialogs carry. Measured in a real browser on this
// guest, two dialogs from two unrelated Cockpit packages (`/users`'s create
// account, `/sosreport`'s run report) both render exactly
// `pf-v6-c-modal-box pf-m-align-top pf-m-md`: 840px wide with 305px gutters
// in a 1449px frame. Without a size modifier, modal-box's own defaults
// (`--Width: 100%`, `--MaxWidth: calc(100% - spacer--xl)`) make the box
// full-bleed -- ours measured 1417px in that same frame, so a two-button
// confirmation stretched the entire viewport. PatternFly's placement
// guidance is the reason for `align-top` specifically: top alignment is for
// modals "with expanding content", and every dialog here grows as its
// command preview expands and its result alert appears.
export const Modal = ({
    title, onClose, children, closeDisabled = false,
}: { title: string; onClose: () => void; children: React.ReactNode; closeDisabled?: boolean }) => (
    <Backdrop>
        <Bullseye>
            <div
                className="pf-v6-c-modal-box pf-m-align-top pf-m-md"
                role="dialog"
                aria-modal="true"
                aria-label={title}
            >
                {/* The close button comes BEFORE the header, which is not a
                    style choice. PatternFly positions `__close` absolutely and
                    reserves room for it with
                    `.pf-v6-c-modal-box__close + * { margin-inline-end: ... }`,
                    so whatever follows the close button is what gets the gap.
                    With the header first, that margin landed on `ModalBody`
                    instead: measured at 390px, the body was needlessly 40px
                    narrower while the title had no reserved space at all and
                    ran underneath the button. "Change compression for group
                    \"shr1\"" rendered as `Change compression for group "s`
                    with the × sitting on top of the last characters, so the
                    group name was the part that disappeared.
                    `createGroupWizard.tsx` already had this order, which is
                    why its dialog never showed the fault. */}
                <div className="pf-v6-c-modal-box__close">
                    <button
                        className="pf-v6-c-button pf-m-plain"
                        type="button"
                        onClick={onClose}
                        disabled={closeDisabled}
                        aria-label={_("common.close")}
                        title={closeDisabled ? inFlightReason() : undefined}
                    >
                        {/* `TimesIcon`, not a literal "×". The element stays a
                            raw <button> for the SSR reason above, but its
                            CONTENT has to match the wizard's close button
                            (createGroupWizard.tsx), which already renders this
                            icon. A text glyph inherits the body font, size and
                            colour instead of the icon token set, so the two
                            dialogs' close buttons did not match each other in
                            either theme. The icon is `aria-hidden`, so the
                            accessible name still comes from `aria-label`.

                            The `pf-v6-c-button__icon` span is what PatternFly's
                            own `Button` puts around an `icon` prop, and it
                            carries the icon's box sizing. Measured in a real
                            browser without it, this button came out 37x30 while
                            the wizard's `<Button variant="plain">` was 37x37 --
                            a smaller touch target for the same control, on the
                            dialogs that include the destructive ones. */}
                        <span className="pf-v6-c-button__icon"><TimesIcon /></span>
                    </button>
                </div>
                <ModalHeader title={<span className={TITLE_WRAP}>{title}</span>} />
                <ModalBody>{children}</ModalBody>
            </div>
        </Bullseye>
    </Backdrop>
);

const CommandPreview = ({ commands }: { commands: string[] }) => {
    const [isExpanded, setIsExpanded] = useState(true);
    return (
        <ExpandableSection
            toggleText={format(ngettext(
                "wizard.commands.toggle.one", "wizard.commands.toggle.other", commands.length,
            ), commands.length)}
            isExpanded={isExpanded}
            onToggle={(_event, expanded) => setIsExpanded(expanded)}
        >
            <CodeBlock>
                <CodeBlockCode className={MONO}>{commands.join("\n")}</CodeBlockCode>
            </CodeBlock>
        </ExpandableSection>
    );
};

const ErrorPanel = ({ message, onClose, onRetry }: { message: string | null; onClose: () => void; onRetry?: () => void }) => (
    <Stack hasGutter>
        <StackItem>
            <Alert variant="danger" isInline title={_("dialogs.error.title")}>
                <p className={MONO}>{message}</p>
            </Alert>
        </StackItem>
        <StackItem>
            <ActionList className={ACTION_ROW}>
                <ActionListItem>
                    <button className="pf-v6-c-button pf-m-secondary" type="button" onClick={onClose}>{_("common.close")}</button>
                </ActionListItem>
                {onRetry && (
                    <ActionListItem>
                        <button className="pf-v6-c-button pf-m-secondary" type="button" onClick={onRetry}>{_("dialogs.retry")}</button>
                    </ActionListItem>
                )}
            </ActionList>
        </StackItem>
    </Stack>
);

const describePreflightBlocker = (b: WritePreflight["blockers"][number]): string => {
    switch (b.kind) {
    case "system_disk":
        return format(_("blocker.systemDisk"), b.name, b.mounts.join(", "));
    case "has_content":
        return format(_("blocker.hasContentShort"), b.name);
    case "no_stable_id":
        return format(_("blocker.noStableId"), b.name);
    case "size_unknown":
        return format(_("blocker.sizeUnknown"), b.name);
    case "not_found":
        return format(_("blocker.notFound"), b.reference);
    default:
        // Covers "unknown" plus any future kind this file doesn't know yet.
        // Lead with plain language so the raw payload reads as supporting
        // detail rather than as the entire message. Still shown, not hidden:
        // a reason we cannot name is worth more than silence.
        return format(_("blocker.unknown"), JSON.stringify(b));
    }
};

// --- scrub -------------------------------------------------------------------

/** What the scrub dialog's speed control offers. `""` is the pre-selected
 * first entry and is NOT a fourth profile: it means "pass no `--priority` at
 * all", which leaves the host-wide kernel speed limit exactly as it is. It is
 * first, and the default, because changing a system-wide setting should be
 * something the operator asks for rather than something a dialog does on
 * their behalf.
 *
 * A function, not a module-scope array, for the same reason as
 * `priorityOptions`: the labels have to be read at render time to follow the
 * session language. */
const scrubSpeedOptions = (): { value: ReshapePriority | ""; label: string }[] => [
    { value: "", label: _("dialogs.scrub.priority.unset") },
    { value: "balanced", label: _("dialogs.scrub.priority.balanced") },
    { value: "background", label: _("dialogs.scrub.priority.background") },
    { value: "max", label: _("dialogs.scrub.priority.max") },
];

/** What this group's bands report they are currently syncing under, for the
 * running-check speed selector to open on. Falls back to `"balanced"` when
 * no band carries a profile -- a check started with no `--priority` at all
 * runs under whatever the system already had, which is not one of the three
 * and cannot be shown as one. */
const runningPriority = (group: GroupStatus): ReshapePriority => {
    const reported = group.bands.map(b => b.sync_priority).find(Boolean);
    return (reported === "background" || reported === "balanced" || reported === "max") ? reported : "balanced";
};

const ScrubDialog = ({ group, onClose, onChanged }: { group: GroupStatus; onClose: () => void; onChanged: () => void }) => {
    const [status, setStatus] = useState<ScrubStatusReport | null>(null);
    const [loadError, setLoadError] = useState<string | null>(null);
    const [loading, setLoading] = useState(true);
    const [action, setAction] = useState<SimpleActionController<string, string> | null>(null);
    const [actionState, setActionState] = useState<SimpleActionState<string> | null>(null);
    const [busy, setBusy] = useState(false);
    const [speed, setSpeed] = useState<ReshapePriority | "">("");
    // The running check's speed is a separate control with a separate
    // in-flight flag: it is not one of the two confirmed actions (start,
    // cancel) this dialog otherwise drives, and it needs no confirmation --
    // nothing is destroyed, nothing is restarted, and the opposite choice is
    // one click away. Seeded from what the bands report they are running at,
    // so the selector opens showing the truth rather than a guess.
    const [liveSpeed, setLiveSpeed] = useState<ReshapePriority>(runningPriority(group));
    const [speedBusy, setSpeedBusy] = useState(false);
    const [speedError, setSpeedError] = useState<string | null>(null);
    // `onChanged` is app.tsx's `refresh`, which synchronously sets
    // `state.kind = "loading"` -- that unmounts this whole dialog (via
    // `Dashboard`) in the same React batch as any setState this dialog's own
    // run handler just made, before it can paint. Deferred to `handleClose`
    // (invoked only when the operator actually dismisses the dialog) so
    // there is nothing left to unmount out from under.
    const [changed, setChanged] = useState(false);
    const handleClose = () => {
        if (changed)
            onChanged();
        onClose();
    };

    const load = () => {
        setLoading(true);
        setLoadError(null);
        const { argv, options } = scrubStatusArgs(group.name);
        cockpit.spawn(argv, options)
                .then(raw => setStatus(parseScrubStatus(raw)))
                .catch(error => setLoadError(spawnErrorMessage(error)))
                .finally(() => setLoading(false));
    };

    // eslint-disable-next-line react-hooks/exhaustive-deps -- runs once per dialog open, group.name is fixed for the dialog's lifetime
    useEffect(() => { load() }, []);

    const begin = (build: (name: string) => ReturnType<typeof scrubStartArgs>) => {
        const controller = new SimpleActionController(cockpit.spawn.bind(cockpit), group.name, build, parseTextResult);
        try {
            setActionState(controller.proceedToConfirm());
            setAction(controller);
        } catch (error) {
            setLoadError(spawnErrorMessage(error));
        }
    };

    const applySpeed = async () => {
        setSpeedBusy(true);
        setSpeedError(null);
        try {
            const { argv, options } = scrubSpeedArgs({ groupName: group.name, priority: liveSpeed });
            await cockpit.spawn(argv, options);
            setChanged(true);
            // The band panel behind this dialog shows the limits and the
            // last throttle decision, so a change here has somewhere to be
            // seen -- reload so it is not stale by the time the dialog
            // closes.
            load();
        } catch (error) {
            setSpeedError(spawnErrorMessage(error));
        } finally {
            setSpeedBusy(false);
        }
    };

    const confirmAndRun = async () => {
        if (!action)
            return;
        action.confirm();
        setBusy(true);
        try {
            const state = await action.execute();
            setActionState(state);
            if (state.step === "done") {
                setAction(null);
                setChanged(true);
                load();
            }
        } finally {
            setBusy(false);
        }
    };

    return (
        <Modal title={format(_("dialogs.scrub.title"), group.name)} onClose={handleClose} closeDisabled={busy}>
            <Stack hasGutter>
                {loading && (
                    <StackItem>
                        <p><Spinner isInline aria-hidden="true" /> {_("dialogs.scrub.loading")}</p>
                    </StackItem>
                )}
                {!loading && loadError && (
                    <StackItem>
                        <Alert variant="danger" isInline title={_("dialogs.loadFailed")}>
                            <p className={MONO}>{loadError}</p>
                        </Alert>
                    </StackItem>
                )}
                {!loading && status && (
                    <StackItem>
                        <DescriptionList orientation={{ md: "horizontal" }} isCompact>
                            <DescriptionListGroup>
                                <DescriptionListTerm>{_("dialogs.scrub.currentState")}</DescriptionListTerm>
                                <DescriptionListDescription>
                                    <strong>{status.running ? _("model.scrub.inProgress") : _("dialogs.scrub.notRunning")}</strong>
                                </DescriptionListDescription>
                            </DescriptionListGroup>
                            <DescriptionListGroup>
                                <DescriptionListTerm>{_("dialogs.scrub.errorsFound")}</DescriptionListTerm>
                                <DescriptionListDescription>
                                    <strong className={MONO}>
                                        {format(ngettext(
                                            "model.scrub.errors.one", "model.scrub.errors.other", status.error_count,
                                        ), status.error_count)}
                                    </strong>
                                </DescriptionListDescription>
                            </DescriptionListGroup>
                        </DescriptionList>
                    </StackItem>
                )}

                {/* The mirror of the selector below, for a check that is
                    ALREADY running: the kernel re-reads these limits as it
                    goes, so a running check can be re-aimed without being
                    stopped, and cancelling one just to run it faster throws
                    away the work already done. Its own button, because
                    unlike start/cancel there is nothing here to confirm. */}
                {!loading && status && status.running && !action && (
                    <StackItem>
                        <FormGroup label={_("dialogs.scrub.speedLabel")} fieldId="scrub-live-speed">
                            <span className="pf-v6-c-form-control">
                                <select
                                    id="scrub-live-speed"
                                    value={liveSpeed}
                                    disabled={speedBusy}
                                    onChange={e => setLiveSpeed(e.target.value as ReshapePriority)}
                                >
                                    {priorityOptions().map(o => <option value={o.value} key={o.value}>{o.label}</option>)}
                                </select>
                            </span>
                            <HelperText><HelperTextItem>{_("dialogs.scrub.speedHint")}</HelperTextItem></HelperText>
                        </FormGroup>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button
                                    className="pf-v6-c-button pf-m-secondary" type="button"
                                    disabled={speedBusy} onClick={applySpeed}
                                >
                                    {speedBusy ? _("dialogs.scrub.speedBusy") : _("dialogs.scrub.speedApply")}
                                </button>
                            </ActionListItem>
                        </ActionList>
                        {speedError && (
                            <HelperText><HelperTextItem variant="error" className={MONO}>{speedError}</HelperTextItem></HelperText>
                        )}
                    </StackItem>
                )}

                {/* Shown under exactly the condition that makes the Start
                    button available, so the control is never offered for a
                    run it cannot affect. */}
                {!loading && status && !status.running && !action && (
                    <StackItem>
                        <FormGroup label={_("dialogs.scrub.priorityLabel")} fieldId="scrub-priority">
                            <span className="pf-v6-c-form-control">
                                <select
                                    id="scrub-priority"
                                    value={speed}
                                    onChange={e => setSpeed(e.target.value as ReshapePriority | "")}
                                >
                                    {scrubSpeedOptions().map(o => <option value={o.value} key={o.value || "unset"}>{o.label}</option>)}
                                </select>
                            </span>
                        </FormGroup>
                    </StackItem>
                )}

                {action && actionState && actionState.step !== "done" && (
                    <StackItem>
                        <Alert
                            variant="danger"
                            isInline
                            title={status?.running ? _("dialogs.scrub.cancelTitle") : _("dialogs.scrub.startTitle")}
                        >
                            <p>{_("dialogs.confirmPrompt")}</p>
                            {actionState.step === "error" && <p className={MONO}>{actionState.errorMessage}</p>}
                        </Alert>
                    </StackItem>
                )}

                <StackItem>
                    <ActionList className={ACTION_ROW}>
                        <ActionListItem>
                            <button
                                className="pf-v6-c-button pf-m-secondary" type="button"
                                onClick={handleClose} disabled={busy} title={busy ? inFlightReason() : undefined}
                            >
                                {_("common.close")}
                            </button>
                        </ActionListItem>
                        <ActionListItem>
                            <button className="pf-v6-c-button pf-m-secondary" type="button" onClick={load} disabled={loading}>{_("common.refresh")}</button>
                        </ActionListItem>
                        {!loading && status && !status.running && !action && (
                            <ActionListItem>
                                <button
                                    className="pf-v6-c-button pf-m-primary" type="button"
                                    onClick={() => begin(name => scrubStartArgs(name, speed || undefined))}
                                >
                                    {_("dialogs.scrub.start")}
                                </button>
                            </ActionListItem>
                        )}
                        {!loading && status && status.running && !action && (
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-danger" type="button" onClick={() => begin(scrubCancelArgs)}>
                                    {_("dialogs.scrub.cancel")}
                                </button>
                            </ActionListItem>
                        )}
                        {action && actionState && actionState.step !== "done" && (
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-danger" type="button" disabled={busy} onClick={confirmAndRun}>
                                    {busy ? _("dialogs.busy.working") : _("dialogs.confirmAndRun")}
                                </button>
                            </ActionListItem>
                        )}
                    </ActionList>
                </StackItem>
            </Stack>
        </Modal>
    );
};

// --- expand --------------------------------------------------------------

// The reshape speed profile `shr-rs expand --priority` accepts.
// "balanced" is listed first and is the pre-selected default, matching
// shr-cli's own `default_value = "balanced"`.
// A function, not a module-scope array: the labels have to be read at render
// time so they follow the session language.
const priorityOptions = (): { value: ReshapePriority; label: string }[] => [
    { value: "balanced", label: _("dialogs.expand.priority.balanced") },
    { value: "background", label: _("dialogs.expand.priority.background") },
    { value: "max", label: _("dialogs.expand.priority.max") },
];

const ExpandDialog = ({
    group, disks, onClose, onChanged,
}: { group: GroupStatus; disks: DiskStatus[]; onClose: () => void; onChanged: () => void }) => {
    const candidates = useMemo(() => filterExpandCandidates(disks), [disks]);
    const [selected, setSelected] = useState<string[]>([]);
    const [forceContent, setForceContent] = useState(false);
    const [priority, setPriority] = useState<ReshapePriority>("balanced");
    const [buildError, setBuildError] = useState<string | null>(null);
    const [controller, setController] = useState<ExpandController | null>(null);
    const [state, setState] = useState<ExpandState | null>(null);
    const [confirmText, setConfirmText] = useState("");
    const [busy, setBusy] = useState(false);
    // See ScrubDialog's identical comment above -- `onChanged` (app.tsx's
    // `refresh`) unmounts this dialog synchronously in the same batch as the
    // "done" setState, so the done panel below never painted. Deferred to
    // `handleClose`, invoked only when the operator dismisses the dialog.
    const [changed, setChanged] = useState(false);
    const handleClose = () => {
        if (changed)
            onChanged();
        onClose();
    };

    const toggle = (name: string) => setSelected(prev => (prev.includes(name) ? prev.filter(n => n !== name) : [...prev, name]));

    const startPreflight = async () => {
        setBuildError(null);
        // `skipScrubCheck: false` is not configurable here on purpose --
        // only the `scrubWarning` step below can turn it on, and only after
        // the engine has refused this specific expand.
        const input: ExpandInput = { groupName: group.name, diskIds: selected, forceContent, priority, skipScrubCheck: false };
        const next = new ExpandController(cockpit.spawn.bind(cockpit), input);
        setBusy(true);
        try {
            const s = await next.runPreflight();
            setController(next);
            setState(s);
        } catch (error) {
            setBuildError(spawnErrorMessage(error));
        } finally {
            setBusy(false);
        }
    };

    const runPreview = async () => {
        if (!controller)
            return;
        setBusy(true);
        try {
            setState(await controller.runPreview());
        } finally {
            setBusy(false);
        }
    };

    // Re-runs the same dry run with the scrub-freshness gate waived. Two
    // deliberate operator actions still stand between here and any write:
    // this button, and the typed group name at the confirm step.
    const acceptScrubRiskAndRetry = async () => {
        if (!controller)
            return;
        controller.acceptScrubCheckRisk();
        await runPreview();
    };

    const setConfirmationText = (text: string) => {
        setConfirmText(text);
        if (controller)
            setState(controller.setConfirmationText(text));
    };

    const execute = async () => {
        if (!controller)
            return;
        setBusy(true);
        try {
            const s = await controller.execute();
            setState(s);
            if (s.step === "done")
                setChanged(true);
        } finally {
            setBusy(false);
        }
    };

    const startOver = () => {
        setController(null);
        setState(null);
        setConfirmText("");
    };

    const step = state?.step ?? "select";
    const canExecute = controller !== null && controller.canExecute();

    return (
        <Modal title={format(_("dialogs.expand.title"), group.name)} onClose={handleClose} closeDisabled={busy}>
            {step === "select" && (
                <Stack hasGutter>
                    <StackItem>
                        <fieldset className={FIELDSET_SHRINK}>
                            <legend>{_("dialogs.expand.legend")}</legend>
                            {candidates.length === 0 && <p><Muted>{_("dialogs.expand.noCandidates")}</Muted></p>}
                            {candidates.map(disk => {
                                // `filterExpandCandidates` deliberately does not
                                // judge system-disk status itself (see its doc comment
                                // in actions.ts) -- `preflight --json` remains the sole
                                // authority for actually blocking one. This only
                                // surfaces what `status --json` already reported
                                // (`disk.system_disk`), the same signal the dashboard's
                                // drive table already renders a warning from, so the
                                // operator sees it at the moment of picking rather than
                                // one step later at the blocked-preflight screen.
                                // Mirrors createGroupWizard.tsx's identical earlier fix for
                                // its own disk picker.
                                //
                                // This row wears PatternFly's own `pf-v6-c-check`
                                // tokens, same as the other two checkboxes in this
                                // file. The disabled state is PF's `pf-m-disabled`
                                // label modifier (patternfly.css:10207), which
                                // replaces the hand-rolled `.wizard-disk-row--disabled`
                                // opacity rule `app.scss` used to carry -- that
                                // stylesheet is gone. Secondary facts (model, size,
                                // the system-disk warning) go in PF's own
                                // `__description` slot rather than being laid out by
                                // hand in a flex row.
                                const disabled = disk.system_disk === true;
                                return (
                                    <label className="pf-v6-c-check" key={disk.name}>
                                        <input
                                            className="pf-v6-c-check__input"
                                            type="checkbox"
                                            checked={selected.includes(disk.name)}
                                            disabled={disabled}
                                            onChange={() => toggle(disk.name)}
                                        />
                                        <span
                                            className={disabled
                                                ? "pf-v6-c-check__label pf-m-disabled"
                                                : "pf-v6-c-check__label"}
                                        >
                                            <span className={MONO}>/dev/{disk.name}</span>
                                        </span>
                                        {/* `FIELDSET_SHRINK` (min-width: 0) again, for the
                                            same reason one level down. `pf-v6-c-check` is a
                                            grid whose second track is `1fr`, and a `1fr`
                                            track's automatic minimum is its content's
                                            min-content width. The system-disk `Badge` below
                                            is a PatternFly `Label`, which does not wrap, so
                                            its min-content held that track open: measured at
                                            390px, this row demanded 340px inside a 322px
                                            fieldset and the badge ran past the dialog edge.
                                            With the minimum released the track shrinks and
                                            the Label falls back to its own ellipsis. */}
                                        <span className={`pf-v6-c-check__description ${FIELDSET_SHRINK}`}>
                                            <span>{disk.model ?? _("wizard.disks.noModel")}</span>{" "}
                                            <span className={MONO}>{formatBytes(disk.size)}</span>
                                            {disabled && (
                                                <>
                                                    {" "}
                                                    <Badge
                                                        tone={{
                                                            label: _("wizard.disks.systemDisk") + (
                                                                disk.system_mounts && disk.system_mounts.length > 0
                                                                    ? ` (${disk.system_mounts.join(", ")})`
                                                                    : ""
                                                            ),
                                                            tone: "warning",
                                                        }}
                                                    />
                                                </>
                                            )}
                                        </span>
                                    </label>
                                );
                            })}
                        </fieldset>
                    </StackItem>
                    <StackItem>
                        <label className="pf-v6-c-check">
                            <input
                                className="pf-v6-c-check__input"
                                type="checkbox"
                                checked={forceContent}
                                onChange={e => setForceContent(e.target.checked)}
                            />
                            <span className="pf-v6-c-check__label">{_("dialogs.expand.forceContent")}</span>
                        </label>
                    </StackItem>
                    <StackItem>
                        <FormGroup label={_("dialogs.expand.priorityLabel")} fieldId="expand-priority">
                            <span className="pf-v6-c-form-control">
                                <select id="expand-priority" value={priority} onChange={e => setPriority(e.target.value as ReshapePriority)}>
                                    {priorityOptions().map(o => <option value={o.value} key={o.value}>{o.label}</option>)}
                                </select>
                            </span>
                        </FormGroup>
                    </StackItem>
                    {buildError && (
                        <StackItem>
                            <HelperText><HelperTextItem variant="error">{buildError}</HelperTextItem></HelperText>
                        </StackItem>
                    )}
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button
                                    className="pf-v6-c-button pf-m-secondary" type="button"
                                    onClick={handleClose} disabled={busy} title={busy ? inFlightReason() : undefined}
                                >
                                    {_("common.cancel")}
                                </button>
                            </ActionListItem>
                            <ActionListItem>
                                <button
                                    className="pf-v6-c-button pf-m-primary" type="button"
                                    disabled={selected.length === 0 || busy}
                                    onClick={startPreflight}
                                >
                                    {busy ? _("wizard.action.preflightBusy") : _("wizard.action.preflight")}
                                </button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}

            {step === "blocked" && state?.preflight && (
                <Stack hasGutter>
                    <StackItem>
                        <Alert variant="danger" isInline title={_("dialogs.expand.blockedTitle")} />
                    </StackItem>
                    <StackItem>
                        <List>
                            {state.preflight.blockers.map((b, i) => <ListItem key={i}>{describePreflightBlocker(b)}</ListItem>)}
                        </List>
                    </StackItem>
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-secondary" type="button" onClick={startOver}>{_("wizard.action.backToDisks")}</button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}

            {step === "preview" && (
                <Stack hasGutter>
                    <StackItem>
                        <p>{_("dialogs.expand.previewIntro")}</p>
                    </StackItem>
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button
                                    className="pf-v6-c-button pf-m-secondary" type="button"
                                    onClick={startOver} disabled={busy} title={busy ? inFlightReason() : undefined}
                                >
                                    {_("wizard.action.back")}
                                </button>
                            </ActionListItem>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-primary" type="button" disabled={busy} onClick={runPreview}>
                                    {busy ? _("wizard.action.previewBusy") : _("wizard.action.preview")}
                                </button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}

            {step === "scrubWarning" && (
                <Stack hasGutter>
                    <StackItem>
                        <Alert variant="warning" isInline title={_("dialogs.expand.scrubWarningTitle")}>
                            <p>{_("dialogs.expand.scrubWarningBody")}</p>
                        </Alert>
                    </StackItem>
                    {/* The engine's own sentence, verbatim -- it names the band
                        and the window, which no translated summary above can. */}
                    <StackItem>
                        <p><Muted>{state?.scrubCheckWarning}</Muted></p>
                    </StackItem>
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button
                                    className="pf-v6-c-button pf-m-secondary" type="button"
                                    onClick={startOver} disabled={busy} title={busy ? inFlightReason() : undefined}
                                >
                                    {_("wizard.action.backToDisks")}
                                </button>
                            </ActionListItem>
                            <ActionListItem>
                                <button
                                    className="pf-v6-c-button pf-m-warning" type="button"
                                    disabled={busy} onClick={acceptScrubRiskAndRetry}
                                >
                                    {busy ? _("wizard.action.previewBusy") : _("dialogs.expand.scrubWarningAccept")}
                                </button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}

            {step === "confirm" && state?.preview && (
                <Stack hasGutter>
                    <StackItem>
                        <Alert variant="danger" isInline title={_("wizard.confirm.title")}>
                            <p>{_("dialogs.expand.confirmBody")}</p>
                        </Alert>
                    </StackItem>
                    {/* A waived safety check has to be visible at the moment of
                        the irreversible click, not only on the step where it
                        was accepted. */}
                    {controller?.scrubCheckSkipped && (
                        <StackItem>
                            <Alert variant="warning" isInline title={_("dialogs.expand.scrubOverridden")} />
                        </StackItem>
                    )}
                    <StackItem>
                        <DescriptionList orientation={{ md: "horizontal" }} isCompact>
                            <DescriptionListGroup>
                                <DescriptionListTerm>{_("dialogs.expand.disksAfter")}</DescriptionListTerm>
                                <DescriptionListDescription>
                                    {format(ngettext(
                                        "wizard.confirm.diskCount.one",
                                        "wizard.confirm.diskCount.other",
                                        state.preview.disk_count,
                                    ), state.preview.disk_count)}
                                </DescriptionListDescription>
                            </DescriptionListGroup>
                        </DescriptionList>
                    </StackItem>
                    <StackItem>
                        <CommandPreview commands={state.preview.planned_commands} />
                    </StackItem>
                    <StackItem>
                        <FormGroup
                            label={format(_("wizard.confirm.typeName"), group.name)}
                            fieldId="expand-confirm-name"
                        >
                            <span className="pf-v6-c-form-control">
                                <input
                                    id="expand-confirm-name" type="text" value={confirmText}
                                    onChange={e => setConfirmationText(e.target.value)} placeholder={group.name}
                                />
                            </span>
                        </FormGroup>
                    </StackItem>
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button
                                    className="pf-v6-c-button pf-m-secondary" type="button"
                                    onClick={startOver} disabled={busy} title={busy ? inFlightReason() : undefined}
                                >
                                    {_("common.cancel")}
                                </button>
                            </ActionListItem>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-danger" type="button" disabled={!canExecute || busy} onClick={execute}>
                                    {busy ? _("dialogs.expand.busy") : _("dialogs.expand.execute")}
                                </button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}

            {step === "executing" && (
                <p><Spinner isInline aria-hidden="true" /> {_("dialogs.expand.executing")}</p>
            )}
            {/* Note (earlier investigation): `state.step` only reaches "executing" after
                the controller's own state does, which happens synchronously before
                the awaited spawn -- but this component's `setState` call only runs
                after that same spawn resolves, so this branch is unreachable in
                practice; `closeDisabled={busy}` on <Modal> above is what actually
                gates the close button during the real in-flight window. Left as-is
                (out of this defect's scope) rather than reworked speculatively. */}

            {step === "done" && state?.result && (
                <Stack hasGutter>
                    <StackItem>
                        <Alert variant="success" isInline title={_("dialogs.expand.doneTitle")} />
                    </StackItem>
                    <StackItem>
                        <p>{format(_("dialogs.expand.doneBody"), state.result.disk_count)}</p>
                    </StackItem>
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-primary" type="button" onClick={handleClose}>{_("common.close")}</button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}

            {step === "error" && <ErrorPanel message={state?.errorMessage ?? null} onClose={handleClose} onRetry={startOver} />}
        </Modal>
    );
};

// --- disk replace ----------------------------------------------------------

// Why a member/candidate disk can't be picked as a replace source or
// target -- see `hasStableId`'s doc comment in actions.ts for the backend
// reasoning. Combined with the system-disk reason (mirrors the
// `ExpandDialog` fix) so both are visible together when both apply.
const describeReplaceUnavailable = (disk: DiskStatus, { checkSystemDisk }: { checkSystemDisk: boolean }): string | null => {
    const reasons: string[] = [];
    if (!hasStableId(disk))
        reasons.push(_("dialogs.replace.noStableId"));
    if (checkSystemDisk && disk.system_disk === true) {
        reasons.push(_("dialogs.replace.systemDisk") + (
            disk.system_mounts && disk.system_mounts.length > 0 ? ` (${disk.system_mounts.join(", ")})` : ""
        ));
    }
    return reasons.length > 0 ? format(_("dialogs.replace.unavailable"), reasons.join(" · ")) : null;
};

export interface ReplaceConfirmStepProps {
    group: GroupStatus;
    oldName: string;
    newName: string;
    replaceInput: ReplaceInput;
    confirmText: string;
    onConfirmText: (text: string) => void;
    state: TypedConfirmState<string>;
    busy: boolean;
    canExecute: boolean;
    onCancel: () => void;
    onExecute: () => void;
}

// Pulled out of `ReplaceDialog` so the "awaiting confirmation" and
// "confirmation was given but execute() just failed" screens are the exact
// same render. Before this split, `ReplaceDialog` only mounted this body
// while `step === "confirm"` -- but `TypedConfirmController.execute()`
// moves `state.step` straight from "confirm" to "error" on failure (never
// back to "confirm"), so the instant a replace failed, this whole body
// (including the `state.step === "error"` paragraph it contained) unmounted
// along with it, leaving the operator looking at a bare `Modal` shell with
// no heading, no body, and no indication anything went wrong -- confirmed
// against a live array: `--old loop10` fails validation, and the dialog
// silently reduces to a title bar and a close button.
// Exported (same reasoning as `Modal`/`ExpandDialog` above) so
// `actionsDialogs.test.ts` can render this exact component with a synthetic
// `state.step === "error"` and a real backend error message, and assert
// that message is actually present in the rendered output -- not merely
// that some internal error-state variable got set.
export const ReplaceConfirmStep = ({
    group, oldName, newName, replaceInput, confirmText, onConfirmText, state, busy, canExecute, onCancel, onExecute,
}: ReplaceConfirmStepProps) => (
    <Stack hasGutter>
        <StackItem>
            <Alert variant="danger" isInline title={_("wizard.confirm.title")}>
                <p>{format(_("dialogs.replace.confirmBody"), `/dev/${oldName}`, `/dev/${newName}`)}</p>
            </Alert>
        </StackItem>
        {/* Built from the same `replaceArgs` the controller itself spawns
            (via `replaceInput`, captured at the moment the confirm step was
            pressed) -- not a hand-written string -- so this preview can
            never drift from what actually runs (it used to be built
            from `oldName`/`newName`, the kernel names, while the real spawn
            used by-id; harmless here since both were wrong the same way,
            but the split invited exactly that kind of drift). */}
        <StackItem>
            <CommandPreview commands={[replaceArgs(replaceInput).argv.join(" ")]} />
        </StackItem>
        <StackItem>
            <FormGroup
                label={format(_("wizard.confirm.typeName"), group.name)}
                fieldId="replace-confirm-name"
            >
                <span className="pf-v6-c-form-control">
                    <input
                        id="replace-confirm-name" type="text" value={confirmText}
                        onChange={e => onConfirmText(e.target.value)} placeholder={group.name}
                    />
                </span>
            </FormGroup>
        </StackItem>
        {state.step === "error" && (
            <StackItem>
                <HelperText><HelperTextItem variant="error" className={MONO}>{state.errorMessage}</HelperTextItem></HelperText>
            </StackItem>
        )}
        <StackItem>
            <ActionList className={ACTION_ROW}>
                <ActionListItem>
                    <button
                        className="pf-v6-c-button pf-m-secondary" type="button"
                        onClick={onCancel} disabled={busy} title={busy ? inFlightReason() : undefined}
                    >
                        {_("common.cancel")}
                    </button>
                </ActionListItem>
                <ActionListItem>
                    <button className="pf-v6-c-button pf-m-danger" type="button" disabled={!canExecute || busy} onClick={onExecute}>
                        {busy ? _("dialogs.replace.busy") : _("dialogs.replace.execute")}
                    </button>
                </ActionListItem>
            </ActionList>
        </StackItem>
    </Stack>
);

const ReplaceDialog = ({
    group, disks, onClose, onChanged,
}: { group: GroupStatus; disks: DiskStatus[]; onClose: () => void; onChanged: () => void }) => {
    const memberDisks = useMemo(() => groupMemberDisks(group, disks), [group, disks]);
    // Default to the first member disk that's actually usable -- picking an
    // id-less member as the default would leave the confirm button permanently
    // disabled with no action the operator could take to fix it.
    const [oldName, setOldName] = useState(memberDisks.find(hasStableId)?.name ?? memberDisks[0]?.name ?? "");
    const oldDisk = memberDisks.find(d => d.name === oldName) ?? null;
    const candidates = useMemo(() => (oldDisk ? filterReplacementCandidates(oldDisk, disks) : []), [oldDisk, disks]);
    const [newName, setNewName] = useState("");
    const newDisk = candidates.find(d => d.name === newName) ?? null;

    const [buildError, setBuildError] = useState<string | null>(null);
    const [controller, setController] = useState<TypedConfirmController<ReplaceInput, string> | null>(null);
    const [state, setState] = useState<TypedConfirmState<string> | null>(null);
    // The exact input the controller was built from -- reused (not rebuilt)
    // by `ReplaceConfirmStep`'s command preview so it can never drift from
    // what `execute()` actually spawns.
    const [pendingInput, setPendingInput] = useState<ReplaceInput | null>(null);
    const [confirmText, setConfirmText] = useState("");
    const [busy, setBusy] = useState(false);
    // See ScrubDialog's identical comment above.
    const [changed, setChanged] = useState(false);
    const handleClose = () => {
        if (changed)
            onChanged();
        onClose();
    };

    const proceed = () => {
        setBuildError(null);
        // `buildReplaceInput` (actions.ts) is the one place that
        // decides `.id` vs `.name` -- this used to be inlined here as
        // `oldDisk?.name`/`newDisk?.name`, which is why Cockpit's disk
        // replace never once matched a real group.
        const input = buildReplaceInput(group.name, oldDisk, newDisk);
        const next = new TypedConfirmController(cockpit.spawn.bind(cockpit), input, replaceArgs, i => i.groupName, parseTextResult);
        try {
            setState(next.proceedToConfirm());
            setController(next);
            setPendingInput(input);
        } catch (error) {
            setBuildError(error instanceof Error ? error.message : String(error));
        }
    };

    const setConfirmationText = (text: string) => {
        setConfirmText(text);
        if (controller)
            setState(controller.setConfirmationText(text));
    };

    const execute = async () => {
        if (!controller)
            return;
        setBusy(true);
        try {
            const s = await controller.execute();
            setState(s);
            if (s.step === "done")
                setChanged(true);
        } finally {
            setBusy(false);
        }
    };

    const startOver = () => {
        setController(null);
        setState(null);
        setPendingInput(null);
        setConfirmText("");
    };

    const step = state?.step ?? "review";
    const canExecute = controller !== null && controller.canExecute();
    // Defense in depth (mirrors this file's other dialogs): even if a
    // disabled <option> were somehow selected, don't let the confirm button
    // proceed with a disk lacking a stable id -- `buildReplaceInput` would
    // otherwise silently send "" for that identifier.
    const canProceed = oldDisk !== null && newDisk !== null && hasStableId(oldDisk) && hasStableId(newDisk);

    return (
        <Modal title={format(_("dialogs.replace.title"), group.name)} onClose={handleClose} closeDisabled={busy}>
            {step === "review" && (
                <Stack hasGutter>
                    <StackItem>
                        <FormGroup label={_("dialogs.replace.oldLabel")} fieldId="replace-old-disk">
                            <span className="pf-v6-c-form-control">
                                <select
                                    id="replace-old-disk" value={oldName}
                                    onChange={e => { setOldName(e.target.value); setNewName("") }}
                                >
                                    {memberDisks.length === 0 && <option value="">{_("dialogs.replace.noMembers")}</option>}
                                    {memberDisks.map(d => {
                                        const reason = describeReplaceUnavailable(d, { checkSystemDisk: false });
                                        return (
                                            <option value={d.name} key={d.name} disabled={reason !== null}>
                                                /dev/{d.name} ({formatBytes(d.size)}){reason ? ` · ${reason}` : ""}
                                            </option>
                                        );
                                    })}
                                </select>
                            </span>
                        </FormGroup>
                    </StackItem>
                    <StackItem>
                        <FormGroup label={_("dialogs.replace.newLabel")} fieldId="replace-new-disk">
                            <span className="pf-v6-c-form-control">
                                <select id="replace-new-disk" value={newName} onChange={e => setNewName(e.target.value)}>
                                    <option value="">{_("dialogs.replace.choose")}</option>
                                    {candidates.map(d => {
                                        // `filterReplacementCandidates` deliberately
                                        // does not judge stable-id or system-disk status
                                        // itself (see its doc comment in actions.ts) --
                                        // both are surfaced here instead, same split as
                                        // ExpandDialog's earlier fix above.
                                        const reason = describeReplaceUnavailable(d, { checkSystemDisk: true });
                                        return (
                                            <option value={d.name} key={d.name} disabled={reason !== null}>
                                                /dev/{d.name} ({formatBytes(d.size)}){reason ? ` · ${reason}` : ""}
                                            </option>
                                        );
                                    })}
                                </select>
                            </span>
                            {oldDisk && candidates.length === 0 && (
                                <HelperText>
                                    <HelperTextItem variant="error">{_("dialogs.replace.noCandidates")}</HelperTextItem>
                                </HelperText>
                            )}
                        </FormGroup>
                    </StackItem>
                    {buildError && (
                        <StackItem>
                            <HelperText><HelperTextItem variant="error">{buildError}</HelperTextItem></HelperText>
                        </StackItem>
                    )}
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-secondary" type="button" onClick={handleClose}>{_("common.cancel")}</button>
                            </ActionListItem>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-primary" type="button" disabled={!canProceed} onClick={proceed}>
                                    {_("dialogs.next.confirm")}
                                </button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}

            {(step === "confirm" || step === "error") && state && pendingInput && (
                <ReplaceConfirmStep
                    group={group}
                    oldName={oldName}
                    newName={newName}
                    replaceInput={pendingInput}
                    confirmText={confirmText}
                    onConfirmText={setConfirmationText}
                    state={state}
                    busy={busy}
                    canExecute={canExecute}
                    onCancel={startOver}
                    onExecute={execute}
                />
            )}

            {step === "executing" && (
                <p><Spinner isInline aria-hidden="true" /> {_("dialogs.replace.executing")}</p>
            )}
            {/* See ExpandDialog's identical note: TypedConfirmController.execute()
                sets state.step="executing" synchronously before its awaited spawn,
                but this component's setState only runs after that same await
                resolves, so this branch never actually renders. closeDisabled={busy}
                on <Modal> above is what gates the close button in practice. */}

            {step === "done" && (
                <Stack hasGutter>
                    <StackItem>
                        <Alert variant="success" isInline title={_("dialogs.replace.doneTitle")} />
                    </StackItem>
                    <StackItem>
                        <p className={MONO}>{state?.result}</p>
                    </StackItem>
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-primary" type="button" onClick={handleClose}>{_("common.close")}</button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}
        </Modal>
    );
};

// --- recompress --------------------------------------------------------------

export interface RecompressConfirmStepProps {
    group: GroupStatus;
    compression: string;
    confirmText: string;
    onConfirmText: (text: string) => void;
    state: TypedConfirmState<string>;
    busy: boolean;
    canExecute: boolean;
    onCancel: () => void;
    onExecute: () => void;
}

// Same defect shape an earlier fix addressed for ReplaceDialog -- this body used to be
// inlined directly under `{step === "confirm" && (...)}`, but
// `TypedConfirmController.execute()` moves `state.step` straight from
// "confirm" to "error" on failure (never back to "confirm"), so the whole
// block -- including the `state?.step === "error"` paragraph it contained --
// unmounted the instant a recompress failed, leaving a bare Modal shell with
// no error text. Extracted (same reasoning as `ReplaceConfirmStep`) so
// `actionsDialogs.test.ts` can render this exact component with a synthetic
// `state.step === "error"` and assert the backend's own message is actually
// present in the rendered output.
export const RecompressConfirmStep = ({
    group, compression, confirmText, onConfirmText, state, busy, canExecute, onCancel, onExecute,
}: RecompressConfirmStepProps) => (
    <Stack hasGutter>
        <StackItem>
            <Alert variant="danger" isInline title={_("dialogs.recompress.confirmTitle")}>
                <p>{_("dialogs.recompress.confirmBody")}</p>
            </Alert>
        </StackItem>
        <StackItem>
            <CommandPreview commands={[`shr-rs fs recompress --name ${group.name} --compression ${compression}`]} />
        </StackItem>
        <StackItem>
            <FormGroup
                label={format(_("wizard.confirm.typeName"), group.name)}
                fieldId="recompress-confirm-name"
            >
                <span className="pf-v6-c-form-control">
                    <input
                        id="recompress-confirm-name" type="text" value={confirmText}
                        onChange={e => onConfirmText(e.target.value)} placeholder={group.name}
                    />
                </span>
            </FormGroup>
        </StackItem>
        {state.step === "error" && (
            <StackItem>
                <HelperText><HelperTextItem variant="error" className={MONO}>{state.errorMessage}</HelperTextItem></HelperText>
            </StackItem>
        )}
        <StackItem>
            <ActionList className={ACTION_ROW}>
                <ActionListItem>
                    <button
                        className="pf-v6-c-button pf-m-secondary" type="button"
                        onClick={onCancel} disabled={busy} title={busy ? inFlightReason() : undefined}
                    >
                        {_("common.cancel")}
                    </button>
                </ActionListItem>
                <ActionListItem>
                    <button className="pf-v6-c-button pf-m-danger" type="button" disabled={!canExecute || busy} onClick={onExecute}>
                        {busy ? _("dialogs.recompress.busy") : _("dialogs.recompress.execute")}
                    </button>
                </ActionListItem>
            </ActionList>
        </StackItem>
    </Stack>
);

const RecompressDialog = ({ group, onClose, onChanged }: { group: GroupStatus; onClose: () => void; onChanged: () => void }) => {
    const [compression, setCompression] = useState("zstd:3");
    const [buildError, setBuildError] = useState<string | null>(null);
    const [controller, setController] = useState<TypedConfirmController<RecompressInput, string> | null>(null);
    const [state, setState] = useState<TypedConfirmState<string> | null>(null);
    const [confirmText, setConfirmText] = useState("");
    const [busy, setBusy] = useState(false);
    // See ScrubDialog's identical comment above.
    const [changed, setChanged] = useState(false);
    const handleClose = () => {
        if (changed)
            onChanged();
        onClose();
    };

    const proceed = () => {
        setBuildError(null);
        const input: RecompressInput = { groupName: group.name, compression };
        const next = new TypedConfirmController(cockpit.spawn.bind(cockpit), input, recompressArgs, i => i.groupName, parseTextResult);
        try {
            setState(next.proceedToConfirm());
            setController(next);
        } catch (error) {
            setBuildError(error instanceof Error ? error.message : String(error));
        }
    };

    const setConfirmationText = (text: string) => {
        setConfirmText(text);
        if (controller)
            setState(controller.setConfirmationText(text));
    };

    const execute = async () => {
        if (!controller)
            return;
        setBusy(true);
        try {
            const s = await controller.execute();
            setState(s);
            if (s.step === "done")
                setChanged(true);
        } finally {
            setBusy(false);
        }
    };

    const startOver = () => {
        setController(null);
        setState(null);
        setConfirmText("");
    };

    const step = state?.step ?? "review";
    const canExecute = controller !== null && controller.canExecute();

    return (
        <Modal title={format(_("dialogs.recompress.title"), group.name)} onClose={handleClose} closeDisabled={busy}>
            {step === "review" && (
                <Stack hasGutter>
                    <StackItem>
                        <FormGroup label={_("dialogs.recompress.field")} fieldId="recompress-compression">
                            <span className="pf-v6-c-form-control">
                                <input
                                    id="recompress-compression" type="text" value={compression}
                                    onChange={e => setCompression(e.target.value)} placeholder="zstd:3"
                                />
                            </span>
                        </FormGroup>
                    </StackItem>
                    <StackItem>
                        <Caveat>{_("dialogs.recompress.caveat")}</Caveat>
                    </StackItem>
                    {buildError && (
                        <StackItem>
                            <HelperText><HelperTextItem variant="error">{buildError}</HelperTextItem></HelperText>
                        </StackItem>
                    )}
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-secondary" type="button" onClick={handleClose}>{_("common.cancel")}</button>
                            </ActionListItem>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-primary" type="button" onClick={proceed}>{_("dialogs.next.confirm")}</button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}

            {(step === "confirm" || step === "error") && state && (
                <RecompressConfirmStep
                    group={group}
                    compression={compression}
                    confirmText={confirmText}
                    onConfirmText={setConfirmationText}
                    state={state}
                    busy={busy}
                    canExecute={canExecute}
                    onCancel={startOver}
                    onExecute={execute}
                />
            )}

            {step === "executing" && (
                <p><Spinner isInline aria-hidden="true" /> {_("dialogs.recompress.executing")}</p>
            )}
            {/* See ExpandDialog's identical note: TypedConfirmController.execute()
                sets state.step="executing" synchronously before its awaited spawn,
                but this component's setState only runs after that same await
                resolves, so this branch never actually renders. closeDisabled={busy}
                on <Modal> above is what gates the close button in practice. */}

            {step === "done" && (
                <Stack hasGutter>
                    <StackItem>
                        <Alert variant="success" isInline title={_("dialogs.recompress.doneTitle")} />
                    </StackItem>
                    <StackItem>
                        <p className={MONO}>{state?.result}</p>
                    </StackItem>
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-primary" type="button" onClick={handleClose}>{_("common.close")}</button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}
        </Modal>
    );
};

// --- destroy (tear down a group entirely -- the gap this file closes) -------

export interface DestroyConfirmStepProps {
    group: GroupStatus;
    destroyInput: DestroyInput;
    confirmText: string;
    onConfirmText: (text: string) => void;
    state: TypedConfirmState<DestroyResult>;
    busy: boolean;
    canExecute: boolean;
    onCancel: () => void;
    onExecute: () => void;
}

// Same split as `ReplaceConfirmStep`/`RecompressConfirmStep`: pulled
// out so the "awaiting confirmation" and "confirmation given but execute()
// just failed" screens are the exact same render -- `TypedConfirmController.
// execute()` moves `state.step` straight from "confirm" to "error" on
// failure, never back to "confirm", so a body only mounted under `step ===
// "confirm"` would silently drop the error message the instant a destroy
// failed.
export const DestroyConfirmStep = ({
    group, destroyInput, confirmText, onConfirmText, state, busy, canExecute, onCancel, onExecute,
}: DestroyConfirmStepProps) => (
    <Stack hasGutter>
        <StackItem>
            <Alert variant="danger" isInline title={_("wizard.confirm.title")}>
                <p>{format(_("dialogs.destroy.confirmBody"), group.name)}</p>
            </Alert>
        </StackItem>
        {/* Built from the same `destroyArgs` the controller itself spawns (via
            `destroyInput`, captured at the moment the confirm step was entered) --
            same reasoning as ReplaceConfirmStep's earlier fix -- so this preview
            can never drift from what actually runs, including whether
            --zero-superblocks is present. */}
        <StackItem>
            <CommandPreview commands={[destroyArgs(destroyInput).argv.join(" ")]} />
        </StackItem>
        <StackItem>
            <FormGroup
                label={format(_("wizard.confirm.typeName"), group.name)}
                fieldId="destroy-confirm-name"
            >
                <span className="pf-v6-c-form-control">
                    <input
                        id="destroy-confirm-name" type="text" value={confirmText}
                        onChange={e => onConfirmText(e.target.value)} placeholder={group.name}
                    />
                </span>
            </FormGroup>
        </StackItem>
        {state.step === "error" && (
            <StackItem>
                <HelperText><HelperTextItem variant="error" className={MONO}>{state.errorMessage}</HelperTextItem></HelperText>
            </StackItem>
        )}
        <StackItem>
            <ActionList className={ACTION_ROW}>
                <ActionListItem>
                    <button
                        className="pf-v6-c-button pf-m-secondary" type="button"
                        onClick={onCancel} disabled={busy} title={busy ? inFlightReason() : undefined}
                    >
                        {_("common.cancel")}
                    </button>
                </ActionListItem>
                <ActionListItem>
                    <button className="pf-v6-c-button pf-m-danger" type="button" disabled={!canExecute || busy} onClick={onExecute}>
                        {busy ? _("dialogs.destroy.busy") : _("dialogs.destroy.execute")}
                    </button>
                </ActionListItem>
            </ActionList>
        </StackItem>
    </Stack>
);

const DestroyDialog = ({ group, onClose, onChanged }: { group: GroupStatus; onClose: () => void; onChanged: () => void }) => {
    const [zeroSuperblocks, setZeroSuperblocks] = useState(false);
    const [buildError, setBuildError] = useState<string | null>(null);
    const [controller, setController] = useState<TypedConfirmController<DestroyInput, DestroyResult> | null>(null);
    const [state, setState] = useState<TypedConfirmState<DestroyResult> | null>(null);
    // The exact input the controller was built from -- reused (not rebuilt)
    // by `DestroyConfirmStep`'s command preview so it can never drift from
    // what `execute()` actually spawns (same reasoning as `ReplaceDialog`).
    const [pendingInput, setPendingInput] = useState<DestroyInput | null>(null);
    const [confirmText, setConfirmText] = useState("");
    const [busy, setBusy] = useState(false);
    // See ScrubDialog's identical comment above.
    const [changed, setChanged] = useState(false);
    const handleClose = () => {
        if (changed)
            onChanged();
        onClose();
    };

    const proceed = () => {
        setBuildError(null);
        const input: DestroyInput = { groupName: group.name, zeroSuperblocks };
        const next = new TypedConfirmController(cockpit.spawn.bind(cockpit), input, destroyArgs, i => i.groupName, parseDestroyResult);
        try {
            setState(next.proceedToConfirm());
            setController(next);
            setPendingInput(input);
        } catch (error) {
            setBuildError(error instanceof Error ? error.message : String(error));
        }
    };

    const setConfirmationText = (text: string) => {
        setConfirmText(text);
        if (controller)
            setState(controller.setConfirmationText(text));
    };

    const execute = async () => {
        if (!controller)
            return;
        setBusy(true);
        try {
            const s = await controller.execute();
            setState(s);
            if (s.step === "done")
                setChanged(true);
        } finally {
            setBusy(false);
        }
    };

    const startOver = () => {
        setController(null);
        setState(null);
        setPendingInput(null);
        setConfirmText("");
    };

    const step = state?.step ?? "review";
    const canExecute = controller !== null && controller.canExecute();

    return (
        <Modal title={format(_("dialogs.destroy.title"), group.name)} onClose={handleClose} closeDisabled={busy}>
            {step === "review" && (
                <Stack hasGutter>
                    <StackItem>
                        <Alert variant="danger" isInline title={_("dialogs.destroy.reviewTitle")}>
                            <p>{_("dialogs.destroy.reviewBody")}</p>
                        </Alert>
                    </StackItem>
                    <StackItem>
                        <label className="pf-v6-c-check">
                            <input
                                className="pf-v6-c-check__input"
                                type="checkbox"
                                checked={zeroSuperblocks}
                                onChange={e => setZeroSuperblocks(e.target.checked)}
                            />
                            <span className="pf-v6-c-check__label">{_("dialogs.destroy.zeroSuperblocks")}</span>
                        </label>
                    </StackItem>
                    {buildError && (
                        <StackItem>
                            <HelperText><HelperTextItem variant="error">{buildError}</HelperTextItem></HelperText>
                        </StackItem>
                    )}
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-secondary" type="button" onClick={handleClose}>{_("common.cancel")}</button>
                            </ActionListItem>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-danger" type="button" onClick={proceed}>{_("dialogs.next.confirm")}</button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}

            {(step === "confirm" || step === "error") && state && pendingInput && (
                <DestroyConfirmStep
                    group={group}
                    destroyInput={pendingInput}
                    confirmText={confirmText}
                    onConfirmText={setConfirmationText}
                    state={state}
                    busy={busy}
                    canExecute={canExecute}
                    onCancel={startOver}
                    onExecute={execute}
                />
            )}

            {step === "executing" && (
                <p><Spinner isInline aria-hidden="true" /> {_("dialogs.destroy.executing")}</p>
            )}
            {/* See ExpandDialog's identical note: TypedConfirmController.execute()
                sets state.step="executing" synchronously before its awaited spawn,
                but this component's setState only runs after that same await
                resolves, so this branch never actually renders. closeDisabled={busy}
                on <Modal> above is what gates the close button in practice. */}

            {step === "done" && (
                <Stack hasGutter>
                    <StackItem>
                        <Alert variant="success" isInline title={_("dialogs.destroy.doneTitle")} />
                    </StackItem>
                    <StackItem>
                        <p className={MONO}>{state?.result?.destroyed}</p>
                    </StackItem>
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-primary" type="button" onClick={handleClose}>{_("common.close")}</button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}
        </Modal>
    );
};

// --- snapshot create -----------------------------------------------------

export interface SnapshotConfirmStepProps {
    group: GroupStatus;
    snapshotName: string;
    state: SimpleActionState<string>;
    busy: boolean;
    onCancel: () => void;
    onExecute: () => void;
}

// Same defect shape an earlier fix addressed for ReplaceDialog / RecompressConfirmStep
// above -- `SimpleActionController.execute()` moves `state.step` straight
// from "confirm" to "error" on failure (never back), so mounting this body
// only under `step === "confirm"` unmounted the error paragraph the instant
// a snapshot create failed, leaving a bare Modal shell. Exported for the
// same reason as `RecompressConfirmStep`.
export const SnapshotConfirmStep = ({
    group, snapshotName, state, busy, onCancel, onExecute,
}: SnapshotConfirmStepProps) => (
    <Stack hasGutter>
        <StackItem>
            <CommandPreview commands={[`shr-rs fs snapshot create ${snapshotName} --group ${group.name}`]} />
        </StackItem>
        {state.step === "error" && (
            <StackItem>
                <HelperText><HelperTextItem variant="error" className={MONO}>{state.errorMessage}</HelperTextItem></HelperText>
            </StackItem>
        )}
        <StackItem>
            <ActionList className={ACTION_ROW}>
                <ActionListItem>
                    <button
                        className="pf-v6-c-button pf-m-secondary" type="button"
                        onClick={onCancel} disabled={busy} title={busy ? inFlightReason() : undefined}
                    >
                        {_("common.cancel")}
                    </button>
                </ActionListItem>
                <ActionListItem>
                    <button className="pf-v6-c-button pf-m-primary" type="button" disabled={busy} onClick={onExecute}>
                        {busy ? _("wizard.action.executeBusy") : _("dialogs.snapshot.execute")}
                    </button>
                </ActionListItem>
            </ActionList>
        </StackItem>
    </Stack>
);

const SnapshotDialog = ({ group, onClose, onChanged }: { group: GroupStatus; onClose: () => void; onChanged: () => void }) => {
    const [snapshotName, setSnapshotName] = useState("");
    const [buildError, setBuildError] = useState<string | null>(null);
    const [controller, setController] = useState<SimpleActionController<SnapshotInput, string> | null>(null);
    const [state, setState] = useState<SimpleActionState<string> | null>(null);
    const [busy, setBusy] = useState(false);
    // See ScrubDialog's identical comment above.
    const [changed, setChanged] = useState(false);
    const handleClose = () => {
        if (changed)
            onChanged();
        onClose();
    };

    const proceed = () => {
        setBuildError(null);
        const input: SnapshotInput = { groupName: group.name, snapshotName };
        const next = new SimpleActionController(cockpit.spawn.bind(cockpit), input, snapshotCreateArgs, parseTextResult);
        try {
            setState(next.proceedToConfirm());
            setController(next);
        } catch (error) {
            setBuildError(error instanceof Error ? error.message : String(error));
        }
    };

    const confirmAndRun = async () => {
        if (!controller)
            return;
        controller.confirm();
        setBusy(true);
        try {
            const s = await controller.execute();
            setState(s);
            if (s.step === "done")
                setChanged(true);
        } finally {
            setBusy(false);
        }
    };

    const startOver = () => {
        setController(null);
        setState(null);
    };

    const step = state?.step ?? "review";

    return (
        <Modal title={format(_("dialogs.snapshot.title"), group.name)} onClose={handleClose} closeDisabled={busy}>
            {step === "review" && (
                <Stack hasGutter>
                    <StackItem>
                        <FormGroup label={_("dialogs.snapshot.field")} fieldId="snapshot-name">
                            <span className="pf-v6-c-form-control">
                                <input
                                    id="snapshot-name" type="text" value={snapshotName}
                                    onChange={e => setSnapshotName(e.target.value)} placeholder="before-upgrade"
                                />
                            </span>
                        </FormGroup>
                    </StackItem>
                    <StackItem>
                        <Caveat>{format(_("dialogs.snapshot.caveat"), `@snapshots/${snapshotName || "..."}`)}</Caveat>
                    </StackItem>
                    {buildError && (
                        <StackItem>
                            <HelperText><HelperTextItem variant="error">{buildError}</HelperTextItem></HelperText>
                        </StackItem>
                    )}
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-secondary" type="button" onClick={handleClose}>{_("common.cancel")}</button>
                            </ActionListItem>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-primary" type="button" onClick={proceed}>{_("dialogs.next.confirm")}</button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}

            {(step === "confirm" || step === "error") && state && (
                <SnapshotConfirmStep
                    group={group}
                    snapshotName={snapshotName}
                    state={state}
                    busy={busy}
                    onCancel={startOver}
                    onExecute={confirmAndRun}
                />
            )}

            {step === "done" && (
                <Stack hasGutter>
                    <StackItem>
                        <Alert variant="success" isInline title={_("dialogs.snapshot.doneTitle")} />
                    </StackItem>
                    <StackItem>
                        <p className={MONO}>{state?.result}</p>
                    </StackItem>
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-primary" type="button" onClick={handleClose}>{_("common.close")}</button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            )}
        </Modal>
    );
};

// --- schedule install --------------------------------------------------------

/** What the schedule dialog's speed control offers, same shape and same
 * default as `scrubSpeedOptions`: `""` is not a fourth profile, it means
 * "pass no `--scrub-priority`", leaving `policy.toml`'s own setting (and, if
 * that is unset too, the scheduled check's "touch no kernel parameter"
 * behaviour) alone. */
const schedulePriorityOptions = (): { value: ReshapePriority | ""; label: string }[] => [
    { value: "", label: _("dialogs.schedule.priority.unset") },
    { value: "balanced", label: _("dialogs.schedule.priority.balanced") },
    { value: "background", label: _("dialogs.schedule.priority.background") },
    { value: "max", label: _("dialogs.schedule.priority.max") },
];

const ScheduleDialog = ({ onClose, onChanged }: { onClose: () => void; onChanged: () => void }) => {
    const [priority, setPriority] = useState<ReshapePriority | "">("");
    // The controller is built once but `buildCall` runs at execute time, so
    // the selection has to reach it through a ref rather than through the
    // input captured at construction.
    const priorityRef = useRef<ReshapePriority | "">("");
    priorityRef.current = priority;
    const [controller] = useState(() => new SimpleActionController(
        cockpit.spawn.bind(cockpit),
        undefined,
        () => scheduleInstallArgs(priorityRef.current || undefined),
        parseTextResult,
    ));
    const [state, setState] = useState<SimpleActionState<string>>(() => controller.proceedToConfirm());
    const [busy, setBusy] = useState(false);
    // See ScrubDialog's identical comment above.
    const [changed, setChanged] = useState(false);
    const handleClose = () => {
        if (changed)
            onChanged();
        onClose();
    };

    const run = async () => {
        controller.confirm();
        setBusy(true);
        try {
            const s = await controller.execute();
            setState(s);
            if (s.step === "done")
                setChanged(true);
        } finally {
            setBusy(false);
        }
    };

    return (
        <Modal title={_("dialogs.schedule.title")} onClose={handleClose} closeDisabled={busy}>
            <Stack hasGutter>
                {state.step !== "done" && (
                    <>
                        <StackItem>
                            <p>{_("dialogs.schedule.body")}</p>
                        </StackItem>
                        <StackItem>
                            <FormGroup label={_("dialogs.schedule.priorityLabel")} fieldId="schedule-priority">
                                <span className="pf-v6-c-form-control">
                                    <select
                                        id="schedule-priority"
                                        value={priority}
                                        onChange={e => setPriority(e.target.value as ReshapePriority | "")}
                                        disabled={busy}
                                    >
                                        {schedulePriorityOptions().map(o => <option value={o.value} key={o.value || "unset"}>{o.label}</option>)}
                                    </select>
                                </span>
                            </FormGroup>
                        </StackItem>
                        <StackItem>
                            <CommandPreview commands={[scheduleInstallArgs(priority || undefined).argv.join(" ")]} />
                        </StackItem>
                        {state.step === "error" && (
                            <StackItem>
                                <HelperText><HelperTextItem variant="error" className={MONO}>{state.errorMessage}</HelperTextItem></HelperText>
                            </StackItem>
                        )}
                        <StackItem>
                            <ActionList className={ACTION_ROW}>
                                <ActionListItem>
                                    <button
                                        className="pf-v6-c-button pf-m-secondary" type="button"
                                        onClick={handleClose} disabled={busy} title={busy ? inFlightReason() : undefined}
                                    >
                                        {_("common.cancel")}
                                    </button>
                                </ActionListItem>
                                <ActionListItem>
                                    <button className="pf-v6-c-button pf-m-primary" type="button" disabled={busy} onClick={run}>
                                        {busy ? _("dialogs.schedule.busy") : _("dialogs.schedule.execute")}
                                    </button>
                                </ActionListItem>
                            </ActionList>
                        </StackItem>
                    </>
                )}
                {state.step === "done" && (
                    <>
                        <StackItem>
                            <Alert variant="success" isInline title={_("dialogs.schedule.doneTitle")} />
                        </StackItem>
                        <StackItem>
                            <p className={MONO}>{state.result}</p>
                        </StackItem>
                        <StackItem>
                            <ActionList className={ACTION_ROW}>
                                <ActionListItem>
                                    <button className="pf-v6-c-button pf-m-primary" type="button" onClick={handleClose}>{_("common.close")}</button>
                                </ActionListItem>
                            </ActionList>
                        </StackItem>
                    </>
                )}
            </Stack>
        </Modal>
    );
};

// --- reconcile (finish a resize a prior expand had to defer) ------------

// `shr-rs reconcile` finishes any LVM/Btrfs resize a previous `expand` had
// to defer while its mdadm reshape was still running -- the documented,
// verified remedy for the `resize_pending` warning the dashboard's own
// group/band/fs tables already render (`panels.tsx`, not owned by this
// file). Idempotent and non-destructive (`OrchestrationEngine::reconcile`
// never starts a new destructive action -- see `reconcileArgs`'s doc
// comment in actions.ts), so it gets the same shape as `ScheduleDialog`:
// no group scoping, a single explicit confirm, no typed-name gate.
const ReconcileDialog = ({ onClose, onChanged }: { onClose: () => void; onChanged: () => void }) => {
    const [controller] = useState(() => new SimpleActionController(cockpit.spawn.bind(cockpit), undefined, reconcileArgs, parseTextResult));
    const [state, setState] = useState<SimpleActionState<string>>(() => controller.proceedToConfirm());
    const [busy, setBusy] = useState(false);
    // See ScrubDialog's identical comment above.
    const [changed, setChanged] = useState(false);
    const handleClose = () => {
        if (changed)
            onChanged();
        onClose();
    };

    const run = async () => {
        controller.confirm();
        setBusy(true);
        try {
            const s = await controller.execute();
            setState(s);
            if (s.step === "done")
                setChanged(true);
        } finally {
            setBusy(false);
        }
    };

    return (
        <Modal title={_("dialogs.reconcile.title")} onClose={handleClose} closeDisabled={busy}>
            <Stack hasGutter>
                {state.step !== "done" && (
                    <>
                        <StackItem>
                            <p>{_("dialogs.reconcile.body")}</p>
                        </StackItem>
                        <StackItem>
                            <CommandPreview commands={["shr-rs reconcile"]} />
                        </StackItem>
                        {state.step === "error" && (
                            <StackItem>
                                <HelperText><HelperTextItem variant="error" className={MONO}>{state.errorMessage}</HelperTextItem></HelperText>
                            </StackItem>
                        )}
                        <StackItem>
                            <ActionList className={ACTION_ROW}>
                                <ActionListItem>
                                    <button
                                        className="pf-v6-c-button pf-m-secondary" type="button"
                                        onClick={handleClose} disabled={busy} title={busy ? inFlightReason() : undefined}
                                    >
                                        {_("common.cancel")}
                                    </button>
                                </ActionListItem>
                                <ActionListItem>
                                    <button className="pf-v6-c-button pf-m-primary" type="button" disabled={busy} onClick={run}>
                                        {busy ? _("dialogs.reconcile.busy") : _("dialogs.reconcile.execute")}
                                    </button>
                                </ActionListItem>
                            </ActionList>
                        </StackItem>
                    </>
                )}
                {state.step === "done" && (
                    <>
                        <StackItem>
                            <Alert variant="success" isInline title={_("dialogs.reconcile.doneTitle")} />
                        </StackItem>
                        <StackItem>
                            <p className={MONO}>{state.result}</p>
                        </StackItem>
                        <StackItem>
                            <ActionList className={ACTION_ROW}>
                                <ActionListItem>
                                    <button className="pf-v6-c-button pf-m-primary" type="button" onClick={handleClose}>{_("common.close")}</button>
                                </ActionListItem>
                            </ActionList>
                        </StackItem>
                    </>
                )}
            </Stack>
        </Modal>
    );
};

// --- panel (the single mountable export) -------------------------------------

export interface OperationsPanelProps {
    /** Every SHR group currently known (from `StatusReport.groups`). */
    groups: GroupStatus[];
    /** Every physical disk currently known (from `StatusReport.disks`) --
     * used to compute expand/replace candidates. */
    disks: DiskStatus[];
    /** Called after any action finishes with step `"done"` -- the host
     * page should re-run its own `status --json` refresh so the dashboard
     * reflects the change; this component does not poll or refetch status
     * on its own. */
    onChanged: () => void;
}

type OpenDialog =
    | { kind: "none" }
    | { kind: "scrub"; group: GroupStatus }
    | { kind: "expand"; group: GroupStatus }
    | { kind: "replace"; group: GroupStatus }
    | { kind: "recompress"; group: GroupStatus }
    | { kind: "snapshot"; group: GroupStatus }
    | { kind: "destroy"; group: GroupStatus }
    | { kind: "schedule" }
    | { kind: "reconcile" };

/** The accordion `OperationsPanel` is wrapped in -- same local pattern as
 * `panels.tsx`'s own `Section` (copied rather than imported: that one isn't
 * exported, and duplicating five lines beats reaching across module
 * boundaries for a private helper). `<details open>` became PatternFly's
 * expandable `Card`; `isExpanded` is local state because the old element was
 * uncontrolled too. Dialog modals are rendered by `OperationsPanel` as
 * siblings of this component, not children of it, so collapsing this card
 * can never hide an open dialog. */
const Section = (
    { title, note, defaultExpanded = true, children }: {
        title: string;
        note: React.ReactNode;
        defaultExpanded?: boolean;
        children: React.ReactNode;
    },
) => {
    const [expanded, setExpanded] = useState(defaultExpanded);
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

/**
 * The single mountable component this file exports (this module's whole
 * deliverable). Mount it inside `app.tsx`'s ready-state branch, next to
 * `CreateGroupWizard`, e.g.:
 *
 *   import { OperationsPanel } from "./actionsDialogs.js";
 *   ...
 *   {state.kind === "ready" && (
 *       <OperationsPanel
 *           groups={state.report.groups}
 *           disks={state.report.disks}
 *           onChanged={refresh}
 *       />
 *   )}
 *
 * It renders one action row per group (scrub / expand / replace /
 * recompress / snapshot / destroy) plus a single schedule-install action,
 * and owns all of its own dialog/modal state internally -- the host page
 * only needs to supply the current `groups`/`disks` (from its own
 * `status --json` poll) and a callback to refresh them afterward.
 */
export const OperationsPanel = ({ groups, disks, onChanged }: OperationsPanelProps) => {
    const [open, setOpen] = useState<OpenDialog>({ kind: "none" });
    const close = () => setOpen({ kind: "none" });

    // Names every group the dashboard already warns about via its own
    // resize_pending badge (panels.tsx, not owned here) so the reconcile
    // trigger below is visibly tied to that problem, not just one more
    // unmarked button.
    const pendingGroups = groups.filter(g => g.resize_pending).map(g => g.name);

    return (
        <>
            <Section title={_("dialogs.panel.title")} note={_("dialogs.panel.note")}>
                <Stack hasGutter>
                    {groups.length === 0 && (
                        <StackItem>
                            <Caveat>{_("dialogs.panel.empty")}</Caveat>
                        </StackItem>
                    )}
                    {groups.map(group => (
                        <StackItem key={group.name}>
                            <Card isCompact>
                                <CardTitle><strong className={MONO}>{group.name}</strong></CardTitle>
                                <CardBody>
                                    <ActionList className={ACTION_ROW}>
                                        <ActionListItem>
                                            <button
                                                className="pf-v6-c-button pf-m-secondary" type="button"
                                                onClick={() => setOpen({ kind: "scrub", group })}
                                            >
                                                {_("dialogs.panel.scrub")}
                                            </button>
                                        </ActionListItem>
                                        <ActionListItem>
                                            <button
                                                className="pf-v6-c-button pf-m-secondary" type="button"
                                                onClick={() => setOpen({ kind: "expand", group })}
                                            >
                                                {_("dialogs.panel.expand")}
                                            </button>
                                        </ActionListItem>
                                        <ActionListItem>
                                            <button
                                                className="pf-v6-c-button pf-m-secondary" type="button"
                                                onClick={() => setOpen({ kind: "replace", group })}
                                            >
                                                {_("dialogs.panel.replace")}
                                            </button>
                                        </ActionListItem>
                                        <ActionListItem>
                                            <button
                                                className="pf-v6-c-button pf-m-secondary" type="button"
                                                onClick={() => setOpen({ kind: "snapshot", group })}
                                            >
                                                {_("dialogs.panel.snapshot")}
                                            </button>
                                        </ActionListItem>
                                        <ActionListItem>
                                            <button
                                                className="pf-v6-c-button pf-m-secondary" type="button"
                                                onClick={() => setOpen({ kind: "recompress", group })}
                                            >
                                                {_("dialogs.panel.recompress")}
                                            </button>
                                        </ActionListItem>
                                        <ActionListItem>
                                            <button
                                                className="pf-v6-c-button pf-m-danger" type="button"
                                                onClick={() => setOpen({ kind: "destroy", group })}
                                            >
                                                {_("dialogs.panel.destroy")}
                                            </button>
                                        </ActionListItem>
                                    </ActionList>
                                </CardBody>
                            </Card>
                        </StackItem>
                    ))}
                    {pendingGroups.length > 0 && (
                        <StackItem>
                            <Alert
                                variant="danger"
                                isInline
                                title={format(_("dialogs.panel.pendingTitle"), pendingGroups.join(", "))}
                            >
                                <p>{_("dialogs.panel.pendingBody")}</p>
                            </Alert>
                        </StackItem>
                    )}
                    <StackItem>
                        <ActionList className={ACTION_ROW}>
                            <ActionListItem>
                                <button
                                    className={pendingGroups.length > 0 ? "pf-v6-c-button pf-m-primary" : "pf-v6-c-button pf-m-secondary"}
                                    type="button"
                                    onClick={() => setOpen({ kind: "reconcile" })}
                                >
                                    {_("dialogs.reconcile.title")}
                                </button>
                            </ActionListItem>
                            <ActionListItem>
                                <button className="pf-v6-c-button pf-m-secondary" type="button" onClick={() => setOpen({ kind: "schedule" })}>
                                    {_("dialogs.panel.schedule")}
                                </button>
                            </ActionListItem>
                        </ActionList>
                    </StackItem>
                </Stack>
            </Section>

            {open.kind === "scrub" && <ScrubDialog group={open.group} onClose={close} onChanged={onChanged} />}
            {open.kind === "expand" && <ExpandDialog group={open.group} disks={disks} onClose={close} onChanged={onChanged} />}
            {open.kind === "replace" && <ReplaceDialog group={open.group} disks={disks} onClose={close} onChanged={onChanged} />}
            {open.kind === "recompress" && <RecompressDialog group={open.group} onClose={close} onChanged={onChanged} />}
            {open.kind === "snapshot" && <SnapshotDialog group={open.group} onClose={close} onChanged={onChanged} />}
            {open.kind === "destroy" && <DestroyDialog group={open.group} onClose={close} onChanged={onChanged} />}
            {open.kind === "schedule" && <ScheduleDialog onClose={close} onChanged={onChanged} />}
            {open.kind === "reconcile" && <ReconcileDialog onClose={close} onChanged={onChanged} />}
        </>
    );
};

// Re-exported for convenience so a caller building its own disk pickers
// (e.g. for a replace confirmation summary) doesn't need a second import
// from `actions.ts` just for this one predicate.
export { isValidReplacement };

// Exported so `actionsDialogs.test.ts` can render the real disk
// picker directly and assert its system-disk warning/disabled state in the
// actual output, rather than only exercising the pure argv/filter logic in
// `actions.ts`. `OperationsPanel` remains the only component a host page is
// meant to mount.
export { ExpandDialog };

// Exported (same reasoning as ExpandDialog above) so
// `actionsDialogs.test.ts` can render the real old/new disk pickers and
// assert their disabled/labeled state for id-less and system-disk
// candidates in actual output.
export { ReplaceDialog };

// Exported (same reasoning as ExpandDialog/ReplaceDialog above) so
// `actionsDialogs.test.ts` can render the real review step directly and
// assert the --zero-superblocks checkbox's default-off state and its
// tradeoff wording in actual output.
export { DestroyDialog };
