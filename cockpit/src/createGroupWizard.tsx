/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * The React shell for the SHR group creation wizard. Deliberately thin: all
 * of the safety-relevant decisions (what to spawn, when execution is even
 * allowed, how to interpret a backend failure) live in `createGroup.ts` and
 * are covered by `createGroup.test.ts`. This file is UI wiring only --
 * form fields, the modal chrome, and calling into `CreateGroupController`.
 *
 * The modal chrome below hand-composes `Backdrop` + `Bullseye` + a
 * `pf-v6-c-modal-box` wrapper + `ModalHeader` + `ModalBody`, rather than
 * using PatternFly's own `Modal`. `ModalBox`/`ModalBoxCloseButton` are
 * internal to `@patternfly/react-core` (not part of its public export
 * surface -- only `Modal`, `ModalHeader`, `ModalBody`, `ModalFooter` are),
 * and the real
 * `Modal` renders through `ReactDOM.createPortal` guarded by `canUseDOM`
 * (`typeof window !== "undefined" && window.document && ...`). This
 * project's own JSX test technique (see actionsDialogs.test.ts) stubs
 * `window` without a `document`, so a real `Modal` would unconditionally
 * render `null` there. `Backdrop`, `ModalHeader`, and `ModalBody` are all
 * plain non-portal wrappers (confirmed by reading their source), so they are
 * safe; the box/close-button are recreated with the same literal PatternFly
 * CSS class names PatternFly's own `ModalBox`/`ModalBoxCloseButton` use
 * internally, matching the same technique `ui.tsx` already uses for bare
 * PatternFly utility classes.
 *
 * Steps stay on the existing `CreateGroupController`/`WizardState` machine
 * rather than PatternFly's `Wizard`/`WizardStep`: the preflight-blocked step
 * is a dead end that requires `startOver()` (a full reset to step 1), not
 * "go back one step", and every transition is validation-gated (`canStart`,
 * `canExecute`) rather than a fixed linear step sequence -- PF `Wizard`
 * assumes the latter. A faithful conversion beats an idiomatic one here.
 */

import React, { useEffect, useMemo, useState } from "react";

import {
    ActionList,
    ActionListItem,
    Alert,
    Backdrop,
    Bullseye,
    Button,
    Checkbox,
    DescriptionList,
    DescriptionListDescription,
    DescriptionListGroup,
    DescriptionListTerm,
    ExpandableSection,
    Form,
    FormGroup,
    FormHelperText,
    FormSelect,
    FormSelectOption,
    HelperText,
    HelperTextItem,
    List,
    ListItem,
    ModalBody,
    ModalHeader,
    Spinner,
    Stack,
    StackItem,
    TextInput,
} from "@patternfly/react-core";
import { Table, Tbody, Td, Th, Thead, Tr } from "@patternfly/react-table";
import TimesIcon from "@patternfly/react-icons/dist/esm/icons/times-icon";

import cockpit from "./cockpit.ts";
import {
    CreateGroupController,
    DEFAULT_LV_NAME,
    deriveVgName,
    isConfirmationValid,
    sanitizeLvmNameComponent,
    type CreatedGroupSummary,
    type ExistingGroupIdentity,
    type RedundancyMode,
    type WizardFormInput,
    type WizardState,
} from "./createGroup.ts";
import { _, format, ngettext } from "./i18n.ts";
import { formatBytes, type DiskStatus } from "./model.ts";
import { Caveat, Chip, MONO, Muted } from "./ui.js";

interface Props {
    disks: DiskStatus[];
    existingGroupNames: string[];
    // Every already-recorded group's real VG name, so the wizard can
    // reject a colliding one before `create` ever runs -- see
    // `findVgNameConflict`'s doc comment in createGroup.ts for why this is a
    // best-effort client-side guard, not a substitute for a backend check.
    existingGroupVgNames: ExistingGroupIdentity[];
    onClose: () => void;
    onCreated: (result: CreatedGroupSummary) => void;
}

const DEFAULT_MOUNT = "/mnt/shr_data";

// The literal PatternFly CSS class names for the parts of `ModalBox`/
// `ModalBoxCloseButton` this file recreates without importing those
// (non-exported) components -- see this file's header comment.
//
// The size and placement modifiers match stock Cockpit's own dialogs,
// measured in a real browser rather than chosen by eye -- `/users` and
// `/sosreport`, two unrelated Cockpit packages, both render
// `pf-v6-c-modal-box pf-m-align-top pf-m-md`. See the longer note on
// `actionsDialogs.tsx`'s shared `Modal` for the measurements and for why a
// bare modal-box goes full-bleed. `pf-m-align-top` matters more here than
// anywhere else in this plugin: PatternFly recommends it for modals "with
// expanding content", and this one changes height at every step (disk table
// -> preflight findings -> plan preview).
const MODAL_BOX = "pf-v6-c-modal-box pf-m-align-top pf-m-md";
const MODAL_BOX_CLOSE = "pf-v6-c-modal-box__close";

export const CreateGroupWizard = ({ disks, existingGroupNames, existingGroupVgNames, onClose, onCreated }: Props) => {
    const [name, setName] = useState("");
    const [mode, setMode] = useState<RedundancyMode>("shr");
    const [mountPoint, setMountPoint] = useState(DEFAULT_MOUNT);
    const [selectedDisks, setSelectedDisks] = useState<string[]>([]);
    const [forceContent, setForceContent] = useState(false);
    const [confirmationText, setConfirmationTextLocal] = useState("");

    // VG/LV name is derived deterministically from the group name by
    // default (`deriveVgName`/`DEFAULT_LV_NAME`) so two Cockpit-created
    // groups never both try `vgcreate shr_vg`. `vgNameOverride`/
    // `lvNameOverride` stay `null` until the operator actually edits the
    // advanced field -- while `null`, the effective name tracks `name` live,
    // so typing the group name fills in a sensible VG name without the
    // operator having to do anything. Both are sanitized on every keystroke
    // (not just the derived default) so nothing that reaches `--vg-name`/
    // `--lv-name` can contain a character LVM rejects, regardless of source.
    const [vgNameOverride, setVgNameOverride] = useState<string | null>(null);
    const [lvNameOverride, setLvNameOverride] = useState<string | null>(null);
    const derivedVgName = useMemo(() => deriveVgName(name.trim()), [name]);
    const vgName = vgNameOverride ?? derivedVgName;
    const lvName = lvNameOverride ?? DEFAULT_LV_NAME;

    // A disk the backend already flags as `system_disk` must not stay
    // selected if it becomes one after the operator picked it (e.g. a stale
    // selection from before a refresh) -- the checkbox below is disabled for
    // it going forward, but that alone doesn't retroactively clear a
    // selection already in state.
    useEffect(() => {
        const systemDiskNames = new Set(disks.filter(disk => disk.system_disk).map(disk => disk.name));
        setSelectedDisks(prev => prev.filter(diskName => !systemDiskNames.has(diskName)));
    }, [disks]);

    const [controller, setController] = useState<CreateGroupController | null>(null);
    const [wizardState, setWizardState] = useState<WizardState | null>(null);
    const [busy, setBusy] = useState(false);

    // Local, uncontrolled-`<details>`-equivalent state: the advanced LVM
    // naming section defaults collapsed (the old hand-rolled "wizard-advanced"
    // element had no `open` attribute), the command-preview list defaults
    // expanded (the old "wizard-command-list" element had `open`).
    const [advancedOpen, setAdvancedOpen] = useState(false);
    const [commandsOpen, setCommandsOpen] = useState(true);

    const nameCollides = existingGroupNames.includes(name.trim());
    const canStart = name.trim().length > 0 && !nameCollides && selectedDisks.length > 0;

    const toggleDisk = (diskName: string) => {
        setSelectedDisks(prev => (
            prev.includes(diskName) ? prev.filter(d => d !== diskName) : [...prev, diskName]
        ));
    };

    const startPreflight = async () => {
        const input: WizardFormInput = {
            name: name.trim(),
            mode,
            mountPoint,
            selectedDisks,
            forceContent,
            vgName,
            lvName,
        };
        const next = new CreateGroupController(cockpit.spawn.bind(cockpit), input, existingGroupVgNames);
        setController(next);
        setBusy(true);
        try {
            const state = await next.runPreflight();
            setWizardState(state);
        } finally {
            setBusy(false);
        }
    };

    const runPreview = async () => {
        if (!controller)
            return;
        setBusy(true);
        try {
            const state = await controller.runPreview();
            setWizardState(state);
        } finally {
            setBusy(false);
        }
    };

    const setConfirmationText = (text: string) => {
        setConfirmationTextLocal(text);
        if (controller)
            setWizardState(controller.setConfirmationText(text));
    };

    const execute = async () => {
        if (!controller)
            return;
        setBusy(true);
        try {
            const state = await controller.execute();
            setWizardState(state);
            if (state.step === "done" && state.result)
                onCreated(state.result);
        } finally {
            setBusy(false);
        }
    };

    const startOver = () => {
        setController(null);
        setWizardState(null);
        setConfirmationTextLocal("");
        setBusy(false);
    };

    const step = wizardState?.step ?? "select-disks";
    const canExecute = useMemo(
        () => controller !== null && wizardState !== null &&
            wizardState.step === "confirm" && wizardState.preview !== null &&
            isConfirmationValid(confirmationText, name.trim()),
        [controller, wizardState, confirmationText, name],
    );

    return (
        <Backdrop>
            {/* `Bullseye` is what centres the box in the backdrop, and it is
                the same shape `actionsDialogs.tsx`'s `Modal` wrapper (and
                PatternFly's own `ModalBox`) uses. Without it the box was
                laid out flush against the backdrop's start edge: modal-box's
                `--MaxWidth: calc(100% - spacer--xl)` still applied, so all
                32px of the intended gutter collected on the right and none
                on the left. Measured in a real browser at 1449px frame
                width: box left 0, width 1417. */}
            <Bullseye>
                <div className={MODAL_BOX} role="dialog" aria-modal="true" aria-label={_("wizard.title")}>
                    <div className={MODAL_BOX_CLOSE}>
                        <Button variant="plain" onClick={onClose} aria-label={_("common.close")} icon={<TimesIcon />} />
                    </div>
                    <ModalHeader title={_("wizard.title")} />
                    <ModalBody>
                        {step === "select-disks" && (
                            <Stack hasGutter>
                                {/* preventDefault: this Form has no submit action of its own
                                (every button below is type="button") -- without this, Enter
                                inside a text field would trigger the browser's default
                                form-submit navigation, which the old plain-<label> markup
                                never had a way to do. */}
                                <StackItem>
                                    <Form onSubmit={event => event.preventDefault()}>
                                        <FormGroup label={_("wizard.field.name")} fieldId="wizard-name">
                                            <TextInput
                                            id="wizard-name" type="text" value={name}
                                            onChange={(_event, value) => setName(value)}
                                            placeholder="shr1"
                                            validated={nameCollides ? "error" : "default"}
                                            />
                                            {nameCollides && (
                                                <FormHelperText>
                                                    <HelperText>
                                                        <HelperTextItem variant="error">
                                                            {_("wizard.field.nameTaken")}
                                                        </HelperTextItem>
                                                    </HelperText>
                                                </FormHelperText>
                                            )}
                                        </FormGroup>
                                        <FormGroup label={_("wizard.field.mode")} fieldId="wizard-mode">
                                            <FormSelect
                                            id="wizard-mode" value={mode}
                                            onChange={(_event, value) => setMode(value as RedundancyMode)}
                                            >
                                                <FormSelectOption value="shr" label={_("wizard.mode.shr")} />
                                                <FormSelectOption value="shr2" label={_("wizard.mode.shr2")} />
                                            </FormSelect>
                                        </FormGroup>
                                        <FormGroup label={_("wizard.field.mount")} fieldId="wizard-mount">
                                            <TextInput
                                            id="wizard-mount" type="text" value={mountPoint}
                                            onChange={(_event, value) => setMountPoint(value)}
                                            />
                                        </FormGroup>
                                    </Form>
                                </StackItem>

                                <StackItem>
                                    <fieldset>
                                        <legend>{_("wizard.disks.legend")}</legend>
                                        <Table variant="compact" aria-label={_("wizard.disks.legend")}>
                                            <Thead>
                                                <Tr>
                                                    <Th>{_("wizard.disks.col.select")}</Th>
                                                    <Th>{_("wizard.disks.col.node")}</Th>
                                                    <Th>{_("wizard.disks.col.model")}</Th>
                                                    <Th>{_("wizard.disks.col.capacity")}</Th>
                                                    <Th>{_("wizard.disks.col.state")}</Th>
                                                </Tr>
                                            </Thead>
                                            <Tbody>
                                                {disks.length === 0 && (
                                                    <Tr>
                                                        <Td colSpan={5}><Muted>{_("wizard.disks.empty")}</Muted></Td>
                                                    </Tr>
                                                )}
                                                {disks.map(disk => {
                                                // `disk.system_disk` (not a name guess -- see this
                                                // project's recurring "checks something adjacent to what
                                                // it claims" defect class) is the ONLY source of truth for
                                                // whether this row is disabled. The backend's own
                                                // preflight already refuses a system disk unconditionally
                                                // (no override exists), so disabling it here doesn't hide
                                                // any capability -- it just stops offering, then
                                                // one-step-later rejecting, the exact same disk (the real-
                                                // browser defect this fixes: vda was selectable, then
                                                // preflight rejected it as a system disk).
                                                //
                                                // A disk already in a live RAID array (`disk.arrays.length
                                                // > 0`) is deliberately left selectable, unlike the system
                                                // disk: there is no unconditional backend block for it
                                                // (`WriteBlocker` has no "already in RAID" kind at all --
                                                // it would surface as `has_content`, which the operator
                                                // can already explicitly bypass via the checkbox below).
                                                // an earlier fix documents that reusing a disk pulled from a
                                                // destroyed group is a real, if risky, workflow the
                                                // backend allows -- disabling it here would remove that
                                                // without a backend decision to match, which the phase
                                                // brief explicitly says not to do silently.
                                                    const disabled = disk.system_disk === true;
                                                    return (
                                                        <Tr key={disk.name} {...(disabled ? { className: "pf-v6-u-text-color-disabled" } : {})}>
                                                            <Td dataLabel={_("wizard.disks.col.select")}>
                                                                <Checkbox
                                                                id={`wizard-disk-${disk.name}`}
                                                                aria-label={format(_("wizard.disks.selectAria"), disk.name)}
                                                                isChecked={selectedDisks.includes(disk.name)}
                                                                isDisabled={disabled}
                                                                onChange={() => toggleDisk(disk.name)}
                                                                />
                                                            </Td>
                                                            <Td dataLabel={_("wizard.disks.col.node")} className={MONO}>/dev/{disk.name}</Td>
                                                            <Td dataLabel={_("wizard.disks.col.model")}>{disk.model ?? _("wizard.disks.noModel")}</Td>
                                                            <Td dataLabel={_("wizard.disks.col.capacity")} className={MONO}>{formatBytes(disk.size)}</Td>
                                                            <Td dataLabel={_("wizard.disks.col.state")}>
                                                                {disabled && (
                                                                    <Chip color="orange">
                                                                        {_("wizard.disks.systemDisk")}
                                                                        {disk.system_mounts && disk.system_mounts.length > 0 &&
                                                                        ` (${disk.system_mounts.join(", ")})`}
                                                                    </Chip>
                                                                )}
                                                                {!disabled && disk.arrays.length > 0 && (
                                                                    <Chip>{_("wizard.disks.alreadyInRaid")}</Chip>
                                                                )}
                                                            </Td>
                                                        </Tr>
                                                    );
                                                })}
                                            </Tbody>
                                        </Table>
                                    </fieldset>
                                </StackItem>

                                <StackItem>
                                    <ExpandableSection
                                    toggleText={_("wizard.advanced.toggle")}
                                    isExpanded={advancedOpen}
                                    onToggle={() => setAdvancedOpen(value => !value)}
                                    >
                                        {/* The two derived names used to be `<code>` elements inside
                                            the sentence. They are ordinary `$0`/`$1` substitutions
                                            now: a translator has to be free to move them, and a
                                            catalogue entry carrying markup cannot be moved safely. */}
                                        <Caveat>
                                            {format(_("wizard.advanced.caveat"), derivedVgName, DEFAULT_LV_NAME)}
                                        </Caveat>
                                        <Form onSubmit={event => event.preventDefault()}>
                                            <FormGroup label={_("wizard.field.vgName")} fieldId="wizard-vg-name">
                                                <TextInput
                                                id="wizard-vg-name" type="text" value={vgName}
                                                // An empty field means "go back to the derived
                                                // default", not "use the literal empty string" --
                                                // otherwise clearing the field to retype would get
                                                // stuck sanitizing "" into the "grp" fallback on
                                                // every keystroke instead of tracking `name` again.
                                                onChange={(_event, value) => setVgNameOverride(value === "" ? null : sanitizeLvmNameComponent(value))}
                                                placeholder={derivedVgName}
                                                />
                                            </FormGroup>
                                            <FormGroup label={_("wizard.field.lvName")} fieldId="wizard-lv-name">
                                                <TextInput
                                                id="wizard-lv-name" type="text" value={lvName}
                                                onChange={(_event, value) => setLvNameOverride(value === "" ? null : sanitizeLvmNameComponent(value))}
                                                placeholder={DEFAULT_LV_NAME}
                                                />
                                            </FormGroup>
                                        </Form>
                                    </ExpandableSection>
                                </StackItem>

                                <StackItem>
                                    <Checkbox
                                    id="wizard-force-content"
                                    isChecked={forceContent}
                                    onChange={(_event, checked) => setForceContent(checked)}
                                    label={_("wizard.forceContent")}
                                    />
                                </StackItem>

                                <StackItem>
                                    <Caveat>{_("wizard.selectCaveat")}</Caveat>
                                </StackItem>

                                <StackItem>
                                    <ActionList>
                                        <ActionListItem>
                                            <Button variant="secondary" onClick={onClose}>{_("common.cancel")}</Button>
                                        </ActionListItem>
                                        <ActionListItem>
                                            <Button
                                            variant="primary"
                                            isDisabled={!canStart || busy}
                                            onClick={startPreflight}
                                            >
                                                {busy ? _("wizard.action.preflightBusy") : _("wizard.action.preflight")}
                                            </Button>
                                        </ActionListItem>
                                    </ActionList>
                                </StackItem>
                            </Stack>
                        )}

                        {step === "preflight" && wizardState?.preflight && (
                            <Stack hasGutter>
                                <StackItem>
                                    <Alert variant="danger" isInline title={_("wizard.preflight.blockedTitle")}>
                                        {wizardState.preflight.blockers.length > 0 && (
                                            <List>
                                                {wizardState.preflight.blockers.map((blocker, i) => (
                                                    <ListItem key={i}>{describeBlocker(blocker)}</ListItem>
                                                ))}
                                            </List>
                                        )}
                                        {/* A VG-name collision is a wizard-side check, not one of
                                        the backend's `WriteBlocker` kinds -- shown separately from
                                        `blockers` above rather than invented as a fake one (see
                                        `WizardState.nameConflict`'s doc comment). Can appear even
                                        when `blockers` is empty (the disks themselves were fine). */}
                                        {wizardState.nameConflict && (
                                            <List>
                                                <ListItem>{wizardState.nameConflict}</ListItem>
                                            </List>
                                        )}
                                        {wizardState.preflight.warnings.length > 0 && (
                                            <List>
                                                {wizardState.preflight.warnings.map((warning, i) => <ListItem key={i}>{warning}</ListItem>)}
                                            </List>
                                        )}
                                    </Alert>
                                </StackItem>
                                <StackItem>
                                    <ActionList>
                                        <ActionListItem>
                                            <Button variant="secondary" onClick={startOver}>
                                                {_("wizard.action.backToDisks")}
                                            </Button>
                                        </ActionListItem>
                                    </ActionList>
                                </StackItem>
                            </Stack>
                        )}

                        {step === "preview" && (
                            <Stack hasGutter>
                                <StackItem>
                                    <p>{_("wizard.preview.intro")}</p>
                                </StackItem>
                                <StackItem>
                                    <ActionList>
                                        <ActionListItem>
                                            <Button variant="secondary" onClick={startOver}>{_("wizard.action.back")}</Button>
                                        </ActionListItem>
                                        <ActionListItem>
                                            <Button
                                            variant="primary"
                                            isDisabled={busy}
                                            onClick={runPreview}
                                            >
                                                {busy ? _("wizard.action.previewBusy") : _("wizard.action.preview")}
                                            </Button>
                                        </ActionListItem>
                                    </ActionList>
                                </StackItem>
                            </Stack>
                        )}

                        {step === "confirm" && wizardState?.preview && (
                            <Stack hasGutter>
                                <StackItem>
                                    <Alert variant="danger" isInline title={_("wizard.confirm.title")}>
                                        <p>{_("wizard.confirm.body")}</p>
                                    </Alert>
                                </StackItem>

                                <StackItem>
                                    <DescriptionList isHorizontal isCompact>
                                        <DescriptionListGroup>
                                            <DescriptionListTerm>{_("wizard.confirm.group")}</DescriptionListTerm>
                                            <DescriptionListDescription><strong className={MONO}>{wizardState.preview.name}</strong></DescriptionListDescription>
                                        </DescriptionListGroup>
                                        <DescriptionListGroup>
                                            <DescriptionListTerm>{_("wizard.confirm.mode")}</DescriptionListTerm>
                                            <DescriptionListDescription><strong>{wizardState.preview.mode.toUpperCase()}</strong></DescriptionListDescription>
                                        </DescriptionListGroup>
                                        <DescriptionListGroup>
                                            <DescriptionListTerm>{_("wizard.confirm.mount")}</DescriptionListTerm>
                                            <DescriptionListDescription><strong className={MONO}>{wizardState.preview.mount_point}</strong></DescriptionListDescription>
                                        </DescriptionListGroup>
                                        <DescriptionListGroup>
                                            <DescriptionListTerm>{_("wizard.confirm.bands")}</DescriptionListTerm>
                                            <DescriptionListDescription>
                                                {[
                                                    format(ngettext(
                                                        "wizard.confirm.bandCount.one",
                                                        "wizard.confirm.bandCount.other",
                                                        wizardState.preview.bands.length,
                                                    ), wizardState.preview.bands.length),
                                                    format(ngettext(
                                                        "wizard.confirm.diskCount.one",
                                                        "wizard.confirm.diskCount.other",
                                                        wizardState.preview.disk_count,
                                                    ), wizardState.preview.disk_count),
                                                ].join(", ")}
                                            </DescriptionListDescription>
                                        </DescriptionListGroup>
                                    </DescriptionList>
                                </StackItem>

                                <StackItem>
                                    <ExpandableSection
                                    toggleText={format(ngettext(
                                        "wizard.commands.toggle.one",
                                        "wizard.commands.toggle.other",
                                        wizardState.preview.planned_commands.length,
                                    ), wizardState.preview.planned_commands.length)}
                                    isExpanded={commandsOpen}
                                    onToggle={() => setCommandsOpen(value => !value)}
                                    >
                                        <List component="ol">
                                            {wizardState.preview.planned_commands.map((cmd, i) => (
                                                <ListItem key={i} className={MONO}>{cmd}</ListItem>
                                            ))}
                                        </List>
                                    </ExpandableSection>
                                </StackItem>

                                <StackItem>
                                    <Form onSubmit={event => event.preventDefault()}>
                                        <FormGroup
                                        label={format(_("wizard.confirm.typeName"), name.trim())}
                                        fieldId="wizard-confirm-text"
                                        >
                                            <TextInput
                                            id="wizard-confirm-text" type="text" value={confirmationText}
                                            onChange={(_event, value) => setConfirmationText(value)}
                                            placeholder={name.trim()}
                                            />
                                        </FormGroup>
                                    </Form>
                                </StackItem>

                                <StackItem>
                                    <ActionList>
                                        <ActionListItem>
                                            <Button variant="secondary" onClick={startOver}>{_("common.cancel")}</Button>
                                        </ActionListItem>
                                        <ActionListItem>
                                            <Button
                                            variant="danger"
                                            isDisabled={!canExecute || busy}
                                            onClick={execute}
                                            >
                                                {busy ? _("wizard.action.executeBusy") : _("wizard.action.execute")}
                                            </Button>
                                        </ActionListItem>
                                    </ActionList>
                                </StackItem>
                            </Stack>
                        )}

                        {step === "executing" && (
                            <Stack hasGutter>
                                <StackItem>
                                    <p><Spinner size="md" /> {_("wizard.executing")}</p>
                                </StackItem>
                            </Stack>
                        )}

                        {step === "done" && wizardState?.result && (
                            <Stack hasGutter>
                                <StackItem>
                                    <Alert
                                        variant="success"
                                        isInline
                                        title={format(_("wizard.done.title"), wizardState.result.name)}
                                    />
                                </StackItem>
                                <StackItem>
                                    <p>
                                        {[
                                            format(_("wizard.done.mode"), wizardState.result.mode.toUpperCase()),
                                            format(ngettext(
                                                "wizard.confirm.bandCount.one",
                                                "wizard.confirm.bandCount.other",
                                                wizardState.result.band_count,
                                            ), wizardState.result.band_count),
                                            format(ngettext(
                                                "wizard.confirm.diskCount.one",
                                                "wizard.confirm.diskCount.other",
                                                wizardState.result.disk_count,
                                            ), wizardState.result.disk_count),
                                        ].join(" · ")}
                                    </p>
                                </StackItem>
                                <StackItem>
                                    <ActionList>
                                        <ActionListItem>
                                            <Button variant="primary" onClick={onClose}>{_("common.close")}</Button>
                                        </ActionListItem>
                                    </ActionList>
                                </StackItem>
                            </Stack>
                        )}

                        {step === "error" && (
                            <Stack hasGutter>
                                <StackItem>
                                    <Alert variant="danger" isInline title={_("wizard.error.title")}>
                                        <p>{wizardState?.errorMessage}</p>
                                    </Alert>
                                </StackItem>
                                <StackItem>
                                    <ActionList>
                                        <ActionListItem>
                                            <Button variant="secondary" onClick={onClose}>{_("common.close")}</Button>
                                        </ActionListItem>
                                        <ActionListItem>
                                            <Button variant="secondary" onClick={startOver}>
                                                {_("wizard.action.startOver")}
                                            </Button>
                                        </ActionListItem>
                                    </ActionList>
                                </StackItem>
                            </Stack>
                        )}
                    </ModalBody>
                </div>
            </Bullseye>
        </Backdrop>
    );
};

const describeBlocker = (blocker: { kind: string; name?: string; reference?: string; mounts?: string[]; id?: string }): string => {
    switch (blocker.kind) {
    case "system_disk":
        return format(_("blocker.systemDisk"), blocker.name, (blocker.mounts ?? []).join(", "));
    case "has_content":
        return format(_("blocker.hasContent"), blocker.name);
    case "no_stable_id":
        return format(_("blocker.noStableId"), blocker.name);
    case "size_unknown":
        return format(_("blocker.sizeUnknown"), blocker.name);
    case "not_found":
        return format(_("blocker.notFound"), blocker.reference);
    default:
        // Covers "unknown" plus any future kind this file doesn't know yet.
        // Same shape as actionsDialogs.tsx's describePreflightBlocker: plain
        // language first, raw payload as trailing detail.
        return format(_("blocker.unknown"), JSON.stringify(blocker));
    }
};
