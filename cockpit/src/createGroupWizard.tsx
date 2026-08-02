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
                <div className={MODAL_BOX} role="dialog" aria-modal="true" aria-label="SHR 그룹 만들기">
                    <div className={MODAL_BOX_CLOSE}>
                        <Button variant="plain" onClick={onClose} aria-label="닫기" icon={<TimesIcon />} />
                    </div>
                    <ModalHeader title="SHR 그룹 만들기" />
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
                                        <FormGroup label="그룹 이름" fieldId="wizard-name">
                                            <TextInput
                                            id="wizard-name" type="text" value={name}
                                            onChange={(_event, value) => setName(value)}
                                            placeholder="shr1"
                                            validated={nameCollides ? "error" : "default"}
                                            />
                                            {nameCollides && (
                                                <FormHelperText>
                                                    <HelperText>
                                                        <HelperTextItem variant="error">이미 존재하는 그룹 이름입니다.</HelperTextItem>
                                                    </HelperText>
                                                </FormHelperText>
                                            )}
                                        </FormGroup>
                                        <FormGroup label="모드" fieldId="wizard-mode">
                                            <FormSelect
                                            id="wizard-mode" value={mode}
                                            onChange={(_event, value) => setMode(value as RedundancyMode)}
                                            >
                                                <FormSelectOption value="shr" label="SHR (단일 패리티)" />
                                                <FormSelectOption value="shr2" label="SHR-2 (이중 패리티)" />
                                            </FormSelect>
                                        </FormGroup>
                                        <FormGroup label="마운트 지점" fieldId="wizard-mount">
                                            <TextInput
                                            id="wizard-mount" type="text" value={mountPoint}
                                            onChange={(_event, value) => setMountPoint(value)}
                                            />
                                        </FormGroup>
                                    </Form>
                                </StackItem>

                                <StackItem>
                                    <fieldset>
                                        <legend>디스크 선택</legend>
                                        <Table variant="compact" aria-label="디스크 선택">
                                            <Thead>
                                                <Tr>
                                                    <Th>선택</Th>
                                                    <Th>노드</Th>
                                                    <Th>모델</Th>
                                                    <Th>용량</Th>
                                                    <Th>상태</Th>
                                                </Tr>
                                            </Thead>
                                            <Tbody>
                                                {disks.length === 0 && (
                                                    <Tr>
                                                        <Td colSpan={5}><Muted>감지된 디스크가 없습니다.</Muted></Td>
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
                                                            <Td dataLabel="선택">
                                                                <Checkbox
                                                                id={`wizard-disk-${disk.name}`}
                                                                aria-label={`/dev/${disk.name} 선택`}
                                                                isChecked={selectedDisks.includes(disk.name)}
                                                                isDisabled={disabled}
                                                                onChange={() => toggleDisk(disk.name)}
                                                                />
                                                            </Td>
                                                            <Td dataLabel="노드" className={MONO}>/dev/{disk.name}</Td>
                                                            <Td dataLabel="모델">{disk.model ?? "모델 정보 없음"}</Td>
                                                            <Td dataLabel="용량" className={MONO}>{formatBytes(disk.size)}</Td>
                                                            <Td dataLabel="상태">
                                                                {disabled && (
                                                                    <Chip color="orange">
                                                                        시스템 디스크 (선택 불가)
                                                                        {disk.system_mounts && disk.system_mounts.length > 0 &&
                                                                        ` (${disk.system_mounts.join(", ")})`}
                                                                    </Chip>
                                                                )}
                                                                {!disabled && disk.arrays.length > 0 && <Chip>이미 RAID 연결됨</Chip>}
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
                                    toggleText="고급: LVM 볼륨 그룹/논리 볼륨 이름"
                                    isExpanded={advancedOpen}
                                    onToggle={() => setAdvancedOpen(value => !value)}
                                    >
                                        <Caveat>
                                            비워두면 그룹 이름에서 자동으로 만듭니다(볼륨 그룹은 <code className={MONO}>{derivedVgName}</code>,
                                            논리 볼륨은 <code className={MONO}>{DEFAULT_LV_NAME}</code>). 볼륨 그룹 이름은 호스트 전체에서
                                            단 하나만 존재할 수 있습니다. 같은 이름을 다시 쓰면 안전 점검 단계에서 거부됩니다.
                                        </Caveat>
                                        <Form onSubmit={event => event.preventDefault()}>
                                            <FormGroup label="볼륨 그룹(VG) 이름" fieldId="wizard-vg-name">
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
                                            <FormGroup label="논리 볼륨(LV) 이름" fieldId="wizard-lv-name">
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
                                    label={
                                        <>
                                            선택한 디스크에 기존 데이터가 있어도 진행. 데이터가 있는 디스크는 기본적으로
                                            안전 점검에서 차단되며, 이 체크박스는 그 차단을 명시적으로 우회합니다.
                                        </>
                                    }
                                    />
                                </StackItem>

                                <StackItem>
                                    <Caveat>
                                        디스크를 고른 뒤에도 안전 점검과 실행 계획 미리보기를 거쳐야만 그룹이
                                        만들어집니다. 이 화면에서 바로 디스크가 지워지지 않습니다.
                                    </Caveat>
                                </StackItem>

                                <StackItem>
                                    <ActionList>
                                        <ActionListItem>
                                            <Button variant="secondary" onClick={onClose}>취소</Button>
                                        </ActionListItem>
                                        <ActionListItem>
                                            <Button
                                            variant="primary"
                                            isDisabled={!canStart || busy}
                                            onClick={startPreflight}
                                            >
                                                {busy ? "안전 점검 실행 중..." : "다음: 안전 점검"}
                                            </Button>
                                        </ActionListItem>
                                    </ActionList>
                                </StackItem>
                            </Stack>
                        )}

                        {step === "preflight" && wizardState?.preflight && (
                            <Stack hasGutter>
                                <StackItem>
                                    <Alert variant="danger" isInline title="안전 점검에서 문제가 발견되어 진행할 수 없습니다.">
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
                                            <Button variant="secondary" onClick={startOver}>디스크 선택으로 돌아가기</Button>
                                        </ActionListItem>
                                    </ActionList>
                                </StackItem>
                            </Stack>
                        )}

                        {step === "preview" && (
                            <Stack hasGutter>
                                <StackItem>
                                    <p>안전 점검을 통과했습니다. 실행 계획 미리보기를 생성합니다 (아직 아무 디스크도 건드리지 않습니다).</p>
                                </StackItem>
                                <StackItem>
                                    <ActionList>
                                        <ActionListItem>
                                            <Button variant="secondary" onClick={startOver}>뒤로</Button>
                                        </ActionListItem>
                                        <ActionListItem>
                                            <Button
                                            variant="primary"
                                            isDisabled={busy}
                                            onClick={runPreview}
                                            >
                                                {busy ? "미리보기 생성 중..." : "실행 계획 미리보기"}
                                            </Button>
                                        </ActionListItem>
                                    </ActionList>
                                </StackItem>
                            </Stack>
                        )}

                        {step === "confirm" && wizardState?.preview && (
                            <Stack hasGutter>
                                <StackItem>
                                    <Alert variant="danger" isInline title="이 작업은 되돌릴 수 없습니다.">
                                        <p>아래 디스크가 파티션되고 포맷됩니다. 기존 데이터가 있다면 전부 사라집니다.</p>
                                    </Alert>
                                </StackItem>

                                <StackItem>
                                    <DescriptionList isHorizontal isCompact>
                                        <DescriptionListGroup>
                                            <DescriptionListTerm>그룹</DescriptionListTerm>
                                            <DescriptionListDescription><strong className={MONO}>{wizardState.preview.name}</strong></DescriptionListDescription>
                                        </DescriptionListGroup>
                                        <DescriptionListGroup>
                                            <DescriptionListTerm>모드</DescriptionListTerm>
                                            <DescriptionListDescription><strong>{wizardState.preview.mode.toUpperCase()}</strong></DescriptionListDescription>
                                        </DescriptionListGroup>
                                        <DescriptionListGroup>
                                            <DescriptionListTerm>마운트</DescriptionListTerm>
                                            <DescriptionListDescription><strong className={MONO}>{wizardState.preview.mount_point}</strong></DescriptionListDescription>
                                        </DescriptionListGroup>
                                        <DescriptionListGroup>
                                            <DescriptionListTerm>밴드</DescriptionListTerm>
                                            <DescriptionListDescription>{wizardState.preview.bands.length}개, 디스크 {wizardState.preview.disk_count}개</DescriptionListDescription>
                                        </DescriptionListGroup>
                                    </DescriptionList>
                                </StackItem>

                                <StackItem>
                                    <ExpandableSection
                                    toggleText={`실행될 명령 (${wizardState.preview.planned_commands.length}개)`}
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
                                        label={
                                            <>
                                                계속하려면 그룹 이름 <strong className={MONO}>{name.trim()}</strong>을(를) 정확히 입력하세요
                                            </>
                                        }
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
                                            <Button variant="secondary" onClick={startOver}>취소</Button>
                                        </ActionListItem>
                                        <ActionListItem>
                                            <Button
                                            variant="danger"
                                            isDisabled={!canExecute || busy}
                                            onClick={execute}
                                            >
                                                {busy ? "생성 중..." : "그룹 생성 실행 (되돌릴 수 없음)"}
                                            </Button>
                                        </ActionListItem>
                                    </ActionList>
                                </StackItem>
                            </Stack>
                        )}

                        {step === "executing" && (
                            <Stack hasGutter>
                                <StackItem>
                                    <p><Spinner size="md" /> 그룹을 생성하는 중입니다. 창을 닫지 마세요.</p>
                                </StackItem>
                            </Stack>
                        )}

                        {step === "done" && wizardState?.result && (
                            <Stack hasGutter>
                                <StackItem>
                                    <Alert variant="success" isInline title={`그룹 "${wizardState.result.name}"이(가) 생성되었습니다.`} />
                                </StackItem>
                                <StackItem>
                                    <p>
                                        모드 {wizardState.result.mode.toUpperCase()} · 밴드 {wizardState.result.band_count}개 ·
                                        디스크 {wizardState.result.disk_count}개
                                    </p>
                                </StackItem>
                                <StackItem>
                                    <ActionList>
                                        <ActionListItem>
                                            <Button variant="primary" onClick={onClose}>닫기</Button>
                                        </ActionListItem>
                                    </ActionList>
                                </StackItem>
                            </Stack>
                        )}

                        {step === "error" && (
                            <Stack hasGutter>
                                <StackItem>
                                    <Alert variant="danger" isInline title="실패했습니다.">
                                        <p>{wizardState?.errorMessage}</p>
                                    </Alert>
                                </StackItem>
                                <StackItem>
                                    <ActionList>
                                        <ActionListItem>
                                            <Button variant="secondary" onClick={onClose}>닫기</Button>
                                        </ActionListItem>
                                        <ActionListItem>
                                            <Button variant="secondary" onClick={startOver}>처음부터 다시 시도</Button>
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
        return `${blocker.name}: 시스템 디스크입니다 (마운트: ${(blocker.mounts ?? []).join(", ")}).`;
    case "has_content":
        return `${blocker.name}: 이미 데이터/파티션이 있습니다. 계속하려면 "기존 데이터가 있어도 진행"을 선택하세요.`;
    case "no_stable_id":
        return `${blocker.name}: 안정적인 식별자(by-id)를 찾을 수 없습니다.`;
    case "size_unknown":
        return `${blocker.name}: 용량을 확인할 수 없습니다.`;
    case "not_found":
        return `${blocker.reference}: 해당하는 디스크를 찾을 수 없습니다.`;
    default:
        // Covers "unknown" plus any future kind this file doesn't know yet.
        // Same shape as actionsDialogs.tsx's describePreflightBlocker: plain
        // language first, raw payload as trailing detail.
        return `이 디스크는 사용할 수 없습니다 (자세한 내용: ${JSON.stringify(blocker)}).`;
    }
};
