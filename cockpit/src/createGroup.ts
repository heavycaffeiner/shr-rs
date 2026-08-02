/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Pure logic for the Cockpit "create SHR group" wizard (the design
 * Stage A). This is the single riskiest surface in this phase:
 * it is the first time a browser session can erase a disk. Every decision
 * that matters for safety lives here, as plain functions/classes with no
 * DOM or `cockpit` dependency, so it can be unit tested directly (see
 * `createGroup.test.ts`) instead of only being exercised by clicking
 * through the real UI.
 *
 * The six design constraints from the design Stage A map onto this file
 * as follows:
 *   1. No execution without preview -- `CreateGroupController.execute()`
 *      throws unless `state.step === "confirm"`, which is only reachable
 *      after `runPreview()` succeeded.
 *   2. `superuser: "require"`, never `"try"` -- every spawn args builder
 *      below returns `SPAWN_OPTIONS`, defined exactly once.
 *   3. `--force-content` is never added unless the caller explicitly passed
 *      `forceContent: true` -- see `dryRunCreateArgs`/`createArgs`.
 *   4. No backend-logic duplication -- this file never recomputes
 *      "is this disk safe"; it only stores and displays whatever
 *      `WritePreflight.ok`/`.blockers` the backend returned.
 *   5. Confirmation is more than one click -- `isConfirmationValid` requires
 *      the operator to type the exact group name, not just check a box.
 *   6. Backend errors never render as success -- `runPreflight`/
 *      `runPreview`/`execute` all catch a rejected spawn and transition to
 *      `step: "error"`, never `"done"`.
 */

import {
    isRecord,
    requireBoolean,
    requireNullableNumber,
    requireNullableString,
    requireNumber,
    requireRecord,
    requireString,
    requireStringArray,
} from "./model.ts";
import type { SpawnOptions } from "./cockpit.ts";

export type RedundancyMode = "shr" | "shr2";

// --- WritePreflight (mirrors shr_inspect::WritePreflight / WriteBlocker) ---

export type WriteBlocker =
    | { kind: "system_disk"; name: string; id: string; mounts: string[] }
    | { kind: "no_stable_id"; name: string }
    | { kind: "not_found"; reference: string }
    | { kind: "size_unknown"; name: string }
    | { kind: "has_content"; name: string }
    // A kind this file doesn't recognise. Never thrown away -- the raw
    // object is kept so the operator still sees the safety message the
    // backend sent, even for a blocker shape added after this file was.
    | { kind: "unknown"; raw: Record<string, unknown> };

export interface PreflightTarget {
    kernel_name: string;
    id: string | null;
    size: number | null;
    system_disk: boolean;
    system_mounts: string[];
    has_content: boolean;
}

export interface WritePreflight {
    ok: boolean;
    blockers: WriteBlocker[];
    warnings: string[];
    targets: PreflightTarget[];
}

const requireArray = (value: unknown, message: string): unknown[] => {
    if (!Array.isArray(value))
        throw new Error(message);
    return value;
};

const parseWriteBlocker = (value: unknown, index: number): WriteBlocker => {
    const blocker = requireRecord(value, `preflight blocker #${index + 1} is not an object.`);
    const kind = requireString(blocker.kind, `preflight blocker #${index + 1} has no "kind".`);
    switch (kind) {
    case "system_disk":
        return {
            kind,
            name: requireString(blocker.name, "system_disk blocker missing name"),
            id: requireString(blocker.id, "system_disk blocker missing id"),
            mounts: requireStringArray(blocker.mounts, "system_disk blocker missing mounts"),
        };
    case "no_stable_id":
        return { kind, name: requireString(blocker.name, "no_stable_id blocker missing name") };
    case "not_found":
        return { kind, reference: requireString(blocker.reference, "not_found blocker missing reference") };
    case "size_unknown":
        return { kind, name: requireString(blocker.name, "size_unknown blocker missing name") };
    case "has_content":
        return { kind, name: requireString(blocker.name, "has_content blocker missing name") };
    default:
        // A blocker is a safety message -- an unrecognized `kind` must
        // still reach the operator with its raw content, not vanish into a
        // thrown error that discards this preflight's other blockers,
        // warnings, and targets along with it.
        return { kind: "unknown", raw: blocker };
    }
};

const parsePreflightTarget = (value: unknown, index: number): PreflightTarget => {
    const target = requireRecord(value, `preflight target #${index + 1} is not an object.`);
    return {
        kernel_name: requireString(target.kernel_name, `preflight target #${index + 1} missing kernel_name`),
        id: requireNullableString(target.id, `preflight target #${index + 1} has an invalid id`),
        size: requireNullableNumber(target.size, `preflight target #${index + 1} has an invalid size`),
        system_disk: requireBoolean(target.system_disk, `preflight target #${index + 1} has an invalid system_disk`),
        system_mounts: requireStringArray(
            target.system_mounts, `preflight target #${index + 1} has invalid system_mounts`,
        ),
        has_content: requireBoolean(target.has_content, `preflight target #${index + 1} has an invalid has_content`),
    };
};

/** Parse `shr-rs preflight --json`'s stdout. Throws on anything that isn't a well-formed report. */
export const parseWritePreflight = (raw: string): WritePreflight => {
    let value: unknown;
    try {
        value = JSON.parse(raw);
    } catch {
        throw new Error("shr-rs가 유효한 preflight JSON을 반환하지 않았습니다.");
    }
    const report = requireRecord(value, "preflight 응답이 객체가 아닙니다.");
    if (typeof report.error === "string")
        throw new Error(report.error);
    return {
        ok: requireBoolean(report.ok, "preflight 응답의 ok 필드가 올바르지 않습니다."),
        blockers: requireArray(report.blockers, "preflight 응답의 blockers 목록이 올바르지 않습니다.")
                .map(parseWriteBlocker),
        warnings: requireStringArray(report.warnings, "preflight 응답의 warnings 목록이 올바르지 않습니다."),
        targets: requireArray(report.targets, "preflight 응답의 targets 목록이 올바르지 않습니다.")
                .map(parsePreflightTarget),
    };
};

// --- Create dry-run preview (ArrayState + planned_commands, D13) ----------

export interface PreviewBand {
    index: number;
    level: string;
    md_name: string;
    usable_bytes: number;
}

export interface CreatePreview {
    name: string;
    mode: string;
    layout_version: number;
    mount_point: string;
    bands: PreviewBand[];
    disk_count: number;
    planned_commands: string[];
}

const parsePreviewBand = (value: unknown, index: number): PreviewBand => {
    const band = requireRecord(value, `밴드 #${index + 1} 정보가 올바르지 않습니다.`);
    return {
        index: requireNumber(band.index, `밴드 #${index + 1}의 인덱스가 올바르지 않습니다.`),
        level: requireString(band.level, `밴드 #${index + 1}의 RAID 레벨이 올바르지 않습니다.`),
        md_name: requireString(band.md_name, `밴드 #${index + 1}의 mdadm 이름이 올바르지 않습니다.`),
        usable_bytes: requireNumber(band.usable_bytes, `밴드 #${index + 1}의 가용 용량이 올바르지 않습니다.`),
    };
};

/** Parse `shr-rs create --dry-run --json`'s stdout (an `ArrayState` with `planned_commands` spliced in). */
export const parseCreatePreview = (raw: string): CreatePreview => {
    let value: unknown;
    try {
        value = JSON.parse(raw);
    } catch {
        throw new Error("shr-rs가 유효한 미리보기 JSON을 반환하지 않았습니다.");
    }
    const report = requireRecord(value, "미리보기 응답이 객체가 아닙니다.");
    if (typeof report.error === "string")
        throw new Error(report.error);
    const filesystem = requireRecord(report.filesystem, "미리보기 응답의 filesystem 정보가 올바르지 않습니다.");
    if (!Array.isArray(report.bands))
        throw new Error("미리보기 응답의 bands 목록이 올바르지 않습니다.");
    if (!Array.isArray(report.disks))
        throw new Error("미리보기 응답의 disks 목록이 올바르지 않습니다.");
    return {
        name: requireString(report.name, "미리보기 응답의 name이 올바르지 않습니다."),
        mode: requireString(report.mode, "미리보기 응답의 mode가 올바르지 않습니다."),
        layout_version: requireNumber(report.layout_version, "미리보기 응답의 layout_version이 올바르지 않습니다."),
        mount_point: requireString(filesystem.mount_point, "미리보기 응답의 mount_point가 올바르지 않습니다."),
        bands: report.bands.map(parsePreviewBand),
        disk_count: report.disks.length,
        planned_commands: requireStringArray(
            report.planned_commands, "미리보기 응답의 planned_commands 목록이 올바르지 않습니다.",
        ),
    };
};

// --- Create result (real, non-dry-run ArrayState) --------------------------

export interface CreatedGroupSummary {
    name: string;
    mode: string;
    layout_version: number;
    disk_count: number;
    band_count: number;
}

/** Parse `shr-rs create --json`'s (non-dry-run) stdout on success. */
export const parseCreatedGroup = (raw: string): CreatedGroupSummary => {
    let value: unknown;
    try {
        value = JSON.parse(raw);
    } catch {
        throw new Error("shr-rs가 유효한 생성 결과 JSON을 반환하지 않았습니다.");
    }
    const report = requireRecord(value, "생성 결과 응답이 객체가 아닙니다.");
    if (typeof report.error === "string")
        throw new Error(report.error);
    if (!Array.isArray(report.disks))
        throw new Error("생성 결과 응답의 disks 목록이 올바르지 않습니다.");
    if (!Array.isArray(report.bands))
        throw new Error("생성 결과 응답의 bands 목록이 올바르지 않습니다.");
    return {
        name: requireString(report.name, "생성 결과 응답의 name이 올바르지 않습니다."),
        mode: requireString(report.mode, "생성 결과 응답의 mode가 올바르지 않습니다."),
        layout_version: requireNumber(report.layout_version, "생성 결과 응답의 layout_version이 올바르지 않습니다."),
        disk_count: report.disks.length,
        band_count: report.bands.length,
    };
};

// --- Spawn argument builders ------------------------------------------------

/**
 * Every spawn this wizard issues uses this exact options object --
 * `superuser: "require"`, never `"try"`. `"try"` can silently run
 * unprivileged and report a false "all clear" preflight/preview for a
 * flow whose whole point is deciding whether it's safe to erase a disk
 *. Defined once so no call site can
 * drift from it.
 */
export const SPAWN_OPTIONS: SpawnOptions = { err: "message", superuser: "require" };

export interface SpawnCall {
    argv: string[];
    options: SpawnOptions;
}

// --- LVM VG/LV naming -------------------------------------------------
//
// Cockpit's real-browser measurement: the execution-plan preview printed
// `vgcreate shr_vg /dev/md1` / `lvcreate -l 100%FREE -n data shr_vg` for
// every group, because `dryRunCreateArgs` never passed `--vg-name`/
// `--lv-name` and `shr-cli` falls back to its own hardcoded defaults
// (`shr_vg`/`data`, `crates/shr-cli/src/lib.rs`). A VG name is a host-wide
// LVM namespace (unlike an LV name, which only needs to be unique within its
// own VG) -- a SECOND Cockpit-created group would try `vgcreate shr_vg`
// against a host that already has one.
//
// Investigated (read-only, `crates/shr-orchestrate/src/engine.rs::create`):
// there is no backend guard for this. `create()` DOES reject a colliding
// group NAME up front (`req.name` against `state.toml`'s existing group
// names, engine.rs:538) before touching any disk, but that is a different
// namespace from the VG name -- two groups named "shr1"/"shr2" both
// defaulting to `--vg-name shr_vg` sail past that check. The actual
// `vgcreate` call (engine.rs:846) doesn't happen until AFTER partitions are
// cut and mdadm arrays are created (steps 5's `journal`), so a colliding VG
// name fails deep into the destructive sequence and unwinds through the same
// `wrap_with_rollback` journal an earlier fix already showed can leave a host in a
// worse state than before the attempt started -- this is a
// partial-apply-then-rollback failure, not a clean upfront validation error.
// A backend-side upfront check (mirroring the existing group-name check, but
// for `req.vg_name`) is still needed; this file only closes the gap it can
// reach without touching `crates/` (deriving a non-colliding default name +
// a client-side reject at the wizard's own preflight step, see
// `findVgNameConflict` below).

/**
 * LVM VG/LV names accept only `[A-Za-z0-9_.+-]`, must not start with `-`/`.`,
 * and must not be empty (`vgcreate(8)`). The wizard's group-name field has
 * no such restriction -- it's an arbitrary string stored in state.toml, and
 * `shr-cli --name` never interacts with LVM -- so a group name containing
 * spaces, a slash, or non-ASCII characters cannot be handed to `--vg-name`
 * verbatim. This sanitizes
 * a group name into the corresponding LVM-safe component instead of letting
 * `vgcreate` reject the whole `create` run partway through (see this
 * section's module comment above for why that failure mode is worse than a
 * clean upfront rejection). Falls back to `"grp"` only when sanitizing
 * removes every character (e.g. a name that is entirely non-ASCII or
 * whitespace) --
 * this is a fixed, documented default, not an invented/guessed value.
 */
export const sanitizeLvmNameComponent = (raw: string): string => {
    const replaced = raw.replace(/[^A-Za-z0-9_.+-]/g, "_");
    const withoutLeadingDashOrDot = replaced.replace(/^[-.]+/, "");
    // LVM's own name length ceiling is 127 bytes minus a `/dev/<vg>/` prefix
    // budget; 48 leaves ample room for the `vg_` prefix and is already far
    // longer than any reasonable group name, so this is a defensive cap, not
    // a limit anyone should expect to hit in practice.
    const truncated = withoutLeadingDashOrDot.slice(0, 48);
    // A result with no surviving alphanumeric character (empty, or entirely
    // underscores/dashes/dots left over from sanitizing an all-non-ASCII
    // name, or "---") is not a meaningfully distinct name -- two different inputs
    // could both collapse to it. Falling back to a fixed default here is
    // honest about that ("this group name had no LVM-safe characters at
    // all") instead of emitting something that LOOKS derived from the input
    // but has silently lost all of it.
    return /[A-Za-z0-9]/.test(truncated) ? truncated : "grp";
};

/** Deterministic default VG name for a group -- see this section's module comment. */
export const deriveVgName = (groupName: string): string => `vg_${sanitizeLvmNameComponent(groupName)}`;

/**
 * LV names only need to be unique WITHIN their own VG (unlike VG names,
 * which share one host-wide namespace) -- since `deriveVgName` already gives
 * every group its own VG, every group can safely default to the same LV
 * name without colliding. Kept as a named constant (not inlined) so
 * `dryRunCreateArgs`/the wizard's advanced-override UI have one shared
 * source of truth for it.
 */
export const DEFAULT_LV_NAME = "data";

/** The minimal shape `findVgNameConflict` needs from an already-recorded group. */
export interface ExistingGroupIdentity {
    name: string;
    vg_name: string;
}

/**
 * Client-side-only guard: `shr-rs status --json` already tells Cockpit every
 * existing group's real `vg_name`, so the wizard can reject an
 * about-to-collide VG name before ever calling `create` -- closing most of
 * the practical risk without a backend change. NOT a substitute for a
 * backend-side check: this can't see a create that started in a different
 * Cockpit/TUI/CLI session after the dashboard's last refresh (the same
 * TOCTOU shape `reverify_targets` exists to close for disk targets on the
 * Rust side -- nothing equivalent exists for VG names there yet, see this
 * section's module comment).
 */
export const findVgNameConflict = (vgName: string, existingGroups: ExistingGroupIdentity[]): string | null => {
    const collision = existingGroups.find(group => group.vg_name === vgName);
    return collision
        ? `볼륨 그룹 이름 "${vgName}"은(는) 이미 그룹 "${collision.name}"이(가) 사용 중입니다. ` +
          "다른 이름을 지정하세요."
        : null;
};

export const preflightArgs = (disks: string[], forceContent: boolean): SpawnCall => ({
    argv: [
        "shr-rs", "preflight",
        "--disks", disks.join(","),
        "--json",
        ...(forceContent ? ["--force-content"] : []),
    ],
    options: SPAWN_OPTIONS,
});

export interface WizardFormInput {
    name: string;
    mode: RedundancyMode;
    mountPoint: string;
    selectedDisks: string[];
    forceContent: boolean;
    // Explicit LVM VG/LV names for this group. `dryRunCreateArgs`
    // always passes both -- see `deriveVgName`/`DEFAULT_LV_NAME` for how the
    // wizard fills these in by default -- so a second Cockpit-created group
    // never falls back to `shr-cli`'s host-wide-default `shr_vg`/`data`
    // (confirmed in a real browser: the preview literally printed `vgcreate
    // shr_vg /dev/md1` for every group, because nothing upstream of this
    // file ever passed `--vg-name`/`--lv-name`).
    vgName: string;
    lvName: string;
}

export const dryRunCreateArgs = (input: WizardFormInput): SpawnCall => ({
    argv: [
        "shr-rs", "create",
        "--mode", input.mode,
        "--disks", input.selectedDisks.join(","),
        "--name", input.name,
        "--mount", input.mountPoint,
        "--vg-name", input.vgName,
        "--lv-name", input.lvName,
        "--dry-run",
        "--json",
        ...(input.forceContent ? ["--force-content"] : []),
    ],
    options: SPAWN_OPTIONS,
});

/**
 * The real, executing call -- deliberately derived from `dryRunCreateArgs`
 * by stripping `--dry-run` rather than built independently, so the command
 * the operator actually confirmed in the preview step and the command that
 * really runs can never drift apart (constraint 1's whole point).
 */
export const createArgs = (input: WizardFormInput): SpawnCall => {
    const preview = dryRunCreateArgs(input);
    return { argv: preview.argv.filter(arg => arg !== "--dry-run"), options: preview.options };
};

// --- Confirmation gate (constraint 5) --------------------------------------

/**
 * The operator must type the exact group name to proceed -- a single
 * "OK"/checkbox click is not enough for an operation this irreversible
 *. Exact, case-sensitive match: no
 * trimming/normalizing that could make a typo pass by accident.
 */
export const isConfirmationValid = (typed: string, groupName: string): boolean => (
    typed.length > 0 && typed === groupName
);

// --- Wizard state machine ---------------------------------------------------

export type WizardStep = "select-disks" | "preflight" | "preview" | "confirm" | "executing" | "done" | "error";

export interface WizardState {
    step: WizardStep;
    preflight: WritePreflight | null;
    preview: CreatePreview | null;
    confirmationText: string;
    result: CreatedGroupSummary | null;
    errorMessage: string | null;
    // Set by `runPreflight()` when the request's VG name collides with
    // an already-recorded group -- kept separate from `preflight` (rather
    // than folded into `preflight.blockers`) because it is NOT one of the
    // backend's `WriteBlocker` kinds (constraint 4: this file must not
    // invent a blocker the backend never sent). A non-null value here holds
    // the wizard on the "preflight" step exactly like a real blocker does --
    // see `runPreflight`.
    nameConflict: string | null;
}

export const initialWizardState = (): WizardState => ({
    step: "select-disks",
    preflight: null,
    preview: null,
    confirmationText: "",
    result: null,
    errorMessage: null,
    nameConflict: null,
});

export interface Spawn {
    (argv: string[], options: SpawnOptions): Promise<string>;
}

const spawnErrorMessage = (error: unknown): string => {
    if (typeof error === "string" && error.trim())
        return error;
    if (isRecord(error) && typeof error.message === "string" && error.message.trim())
        return error.message;
    return "명령 실행에 실패했습니다.";
};

/**
 * Drives one wizard run through preflight -> preview -> confirm -> execute.
 * Holds no reference to `cockpit`/the DOM -- `spawn` is injected so tests
 * can substitute a recording fake and assert exactly what would have been
 * run, in what order, without a real Cockpit session (see
 * `createGroup.test.ts`).
 */
export class CreateGroupController {
    state: WizardState = initialWizardState();
    private readonly spawn: Spawn;
    private readonly input: WizardFormInput;
    private readonly existingGroups: ExistingGroupIdentity[];

    constructor(spawn: Spawn, input: WizardFormInput, existingGroups: ExistingGroupIdentity[] = []) {
        this.spawn = spawn;
        this.input = input;
        this.existingGroups = existingGroups;
    }

    /**
     * Step 1 -> 2: preflight (constraint 4 -- trusts `report.ok` verbatim,
     * never re-derives it). Also runs the VG-name collision check
     * (`findVgNameConflict`) once the backend preflight itself passed --
     * this is deliberately NOT folded into "does the backend say the disks
     * are safe", it's a second, independent reason the wizard can't proceed
     * yet. Either one blocks the same way: the wizard stays on
     * `"preflight"` instead of advancing to `"preview"`.
     */
    async runPreflight(): Promise<WizardState> {
        const { argv, options } = preflightArgs(this.input.selectedDisks, this.input.forceContent);
        try {
            const raw = await this.spawn(argv, options);
            const preflight = parseWritePreflight(raw);
            const nameConflict = preflight.ok ? findVgNameConflict(this.input.vgName, this.existingGroups) : null;
            this.state = {
                ...this.state,
                step: (preflight.ok && !nameConflict) ? "preview" : "preflight",
                preflight,
                nameConflict,
            };
        } catch (error) {
            this.state = { ...this.state, step: "error", errorMessage: spawnErrorMessage(error) };
        }
        return this.state;
    }

    /**
     * Step 2 -> 3: dry-run preview (constraint 1). Only reachable once
     * preflight reported `ok: true` for the exact disks/force-content this
     * request carries -- a blocked preflight leaves the wizard on the
     * `"preflight"` step with no way to skip straight to execution.
     */
    async runPreview(): Promise<WizardState> {
        if (!this.state.preflight?.ok || this.state.nameConflict) {
            throw new Error("preview requested before preflight passed -- this is a wizard bug, not a user error");
        }
        const { argv, options } = dryRunCreateArgs(this.input);
        try {
            const raw = await this.spawn(argv, options);
            const preview = parseCreatePreview(raw);
            this.state = { ...this.state, step: "confirm", preview };
        } catch (error) {
            this.state = { ...this.state, step: "error", errorMessage: spawnErrorMessage(error) };
        }
        return this.state;
    }

    setConfirmationText(text: string): WizardState {
        this.state = { ...this.state, confirmationText: text };
        return this.state;
    }

    /** Constraint 1 + 5 combined: both a completed preview AND a matching typed confirmation are required. */
    canExecute(): boolean {
        return this.state.step === "confirm" &&
            this.state.preview !== null &&
            isConfirmationValid(this.state.confirmationText, this.input.name);
    }

    /**
     * Step 3 -> 4: the real, irreversible spawn (constraint 2:
     * `superuser: "require"` via `createArgs`). Throws rather than silently
     * no-op'ing if called out of order -- a caller bypassing `canExecute()`
     * is a bug in the wizard's own wiring, not a state this function should
     * paper over.
     */
    async execute(): Promise<WizardState> {
        if (!this.canExecute()) {
            throw new Error("execute() called without a completed preview and a matching typed confirmation");
        }
        this.state = { ...this.state, step: "executing" };
        const { argv, options } = createArgs(this.input);
        try {
            const raw = await this.spawn(argv, options);
            const result = parseCreatedGroup(raw);
            this.state = { ...this.state, step: "done", result };
        } catch (error) {
            // Constraint 6: a rejected spawn (backend failure/nonzero exit)
            // must land on "error", never "done" -- the two states are
            // mutually exclusive by construction (only one branch below can
            // run), so there is no code path that shows success for a
            // command that actually failed.
            this.state = { ...this.state, step: "error", errorMessage: spawnErrorMessage(error) };
        }
        return this.state;
    }
}
