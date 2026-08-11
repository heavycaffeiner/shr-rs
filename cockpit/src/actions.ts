/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Pure logic for Cockpit's operational actions -- scrub, expand, disk
 * replace, snapshot create, recompress, and schedule install. Same
 * discipline as `createGroup.ts`: `spawn` is injected (no DOM/
 * `cockpit` global reference here), every argv builder is a pure function
 * pinned by a test, `superuser: "require"` always (never `"try"`), and every
 * stdout parser throws on a shape that doesn't match rather than defaulting
 * past it. `expandDryRunArgs`/`expandArgs` deliberately reuse
 * `createGroup.ts`'s `preflightArgs`/`parseWritePreflight`/`parseCreatePreview`/
 * `parseCreatedGroup` -- `expand --dry-run --json` and a real `create`/
 * `expand` all emit the exact same `ArrayState`(+`planned_commands`) shape
 * (see `crates/shr-cli/src/lib.rs`'s `merge_planned_commands`), so re-parsing
 * it here would just be a second copy of the same validation that could
 * silently drift from the first.
 */

import { _ } from "./i18n.ts";
import { requireBoolean, requireNumber, requireRecord, requireString } from "./model.ts";
import type { DiskStatus, GroupStatus } from "./model.ts";
import type { SpawnOptions } from "./cockpit.ts";
import {
    parseCreatePreview,
    parseCreatedGroup,
    parseWritePreflight,
    preflightArgs,
    type CreatePreview,
    type CreatedGroupSummary,
    type WritePreflight,
} from "./createGroup.ts";

export type { CreatePreview, CreatedGroupSummary, WritePreflight };

/**
 * Every spawn call in this file uses this exact options object --
 * `superuser: "require"`, never `"try"`. Every operation here (scrub,
 * expand, replace, recompress, snapshot, schedule install) either writes to
 * `/etc`/`/var/lib/shr-rs` or touches raw disks; `"try"` would let any of
 * them silently run unprivileged and report a misleading failure instead of
 * a clear permission error. Defined once so no builder below can drift.
 */
export const SPAWN_OPTIONS: SpawnOptions = { err: "message", superuser: "require" };

export interface SpawnCall {
    argv: string[];
    options: SpawnOptions;
}

export interface Spawn {
    (argv: string[], options: SpawnOptions): Promise<string>;
}

/** Never collapses a backend failure into a generic string -- surfaces the
 * engine's own stderr/message verbatim, since (per the brief) this project's
 * error text is itself operator-actionable (e.g. "band 0 (md0) has
 * background activity in progress"). Exported so UI-layer callers doing a
 * plain read-only spawn outside any controller (e.g. fetching scrub status)
 * report failures the same way. */
export const spawnErrorMessage = (error: unknown): string => {
    if (typeof error === "string" && error.trim())
        return error;
    if (typeof error === "object" && error !== null) {
        const message = (error as { message?: unknown }).message;
        if (typeof message === "string" && message.trim())
            return message;
    }
    return _("actions.error.commandFailed");
};

const requireNonEmptyName = (value: string, message: string): string => {
    const trimmed = value.trim();
    if (!trimmed)
        throw new Error(message);
    return trimmed;
};

const requireNonEmptyDiskList = (values: string[], message: string): string[] => {
    const cleaned = values.map(v => v.trim()).filter(v => v.length > 0);
    if (cleaned.length === 0)
        throw new Error(message);
    return cleaned;
};

/** Same rule as `createGroup.ts`'s `isConfirmationValid`: exact,
 * case-sensitive match against the group name -- a checkbox is not enough
 * for an action this irreversible. */
export const isConfirmationValid = (typed: string, expected: string): boolean => (
    typed.length > 0 && typed === expected
);

/** Parses a plain-text stdout result (scrub start/cancel, recompress,
 * snapshot create, schedule install all print human text unconditionally --
 * none of those subcommands has a `--json` branch in `shr-cli`). A
 * successfully resolved spawn means the command ran; there is nothing to
 * validate beyond trimming. */
export const parseTextResult = (raw: string): string => raw.trim();

/**
 * Mirrors `shr-cli`'s `PriorityArg` (`crates/shr-cli/src/lib.rs`) -- the
 * kernel speed profile shared by `expand --priority` (reshape) and `fs scrub
 * start --priority` (the mdadm `check`), which is why it is declared here,
 * above both users, rather than in the expand section it started in.
 *
 * What omitting the flag MEANS differs between the two, and neither default
 * is expressible in this union. `expand` defaults to `"balanced"`, so Cockpit
 * says nothing for that value; `fs scrub start` defaults to touching no
 * kernel parameter at all, so Cockpit passes every value it is given. See
 * each builder.
 */
export type ReshapePriority = "background" | "balanced" | "max";

// --- Scrub (fs scrub start/status/cancel) --------------------------

export interface ScrubStatusReport {
    group_name: string;
    running: boolean;
    error_count: number;
}

/** `shr-rs fs scrub start`, optionally under one of the speed profiles
 * `--priority` accepts.
 *
 * Unlike `expandDryRunArgs`, where "balanced" is the CLI's own default and so
 * is omitted, EVERY profile is passed through here and omitting the flag means
 * something different: `fs scrub start` with no `--priority` deliberately
 * changes no kernel parameter at all, so the scrub inherits whatever
 * host-wide `speed_limit_max` is currently set. That is a real, separate
 * choice the dialog offers ("leave it alone"), not a default worth folding
 * into one of the three profiles -- which is why this takes
 * `ReshapePriority | undefined` rather than defaulting the parameter. */
export const scrubStartArgs = (groupName: string, priority?: ReshapePriority): SpawnCall => ({
    argv: [
        "shr-rs", "fs", "scrub", "start",
        "--name", requireNonEmptyName(groupName, _("actions.error.scrubStartNoName")),
        ...(priority ? ["--priority", priority] : []),
    ],
    options: SPAWN_OPTIONS,
});

export const scrubStatusArgs = (groupName: string): SpawnCall => ({
    argv: [
        "shr-rs", "fs", "scrub", "status",
        "--name", requireNonEmptyName(groupName, _("actions.error.scrubStatusNoName")),
        "--json",
    ],
    options: SPAWN_OPTIONS,
});

export const scrubCancelArgs = (groupName: string): SpawnCall => ({
    argv: ["shr-rs", "fs", "scrub", "cancel", "--name", requireNonEmptyName(groupName, _("actions.error.scrubCancelNoName"))],
    options: SPAWN_OPTIONS,
});

/** `shr-rs fs scrub speed`: re-aim a check that is ALREADY running at a
 * different profile, without stopping it.
 *
 * The profile is required here, unlike `scrubStartArgs`, because there is no
 * "leave it alone" reading of this verb: the operator is asking for a
 * specific speed on work already in progress. The CLI refuses when nothing
 * is running, which is what the dialog surfaces rather than second-guessing
 * from a status poll that may be seconds stale. */
export const scrubSpeedArgs = (input: { groupName: string; priority: ReshapePriority }): SpawnCall => ({
    argv: [
        "shr-rs", "fs", "scrub", "speed",
        "--name", requireNonEmptyName(input.groupName, _("actions.error.scrubSpeedNoName")),
        "--priority", input.priority,
    ],
    options: SPAWN_OPTIONS,
});

/** Parses `shr-rs fs scrub status --name <g> --json`'s stdout -- a single
 * `serde_json::json!` object (`group`/`running`/`error_count`), emitted
 * regardless of whether errors were found (unlike the non-JSON branch,
 * which `bail!`s on `error_count > 0`). */
export const parseScrubStatus = (raw: string): ScrubStatusReport => {
    let value: unknown;
    try {
        value = JSON.parse(raw);
    } catch {
        throw new Error(_("actions.error.scrubNotJson"));
    }
    const report = requireRecord(value, _("actions.error.scrubNotObject"));
    if (typeof report.error === "string")
        throw new Error(report.error);
    return {
        group_name: requireString(report.group, _("actions.error.scrubGroup")),
        running: requireBoolean(report.running, _("actions.error.scrubRunning")),
        error_count: requireNumber(report.error_count, _("actions.error.scrubErrorCount")),
    };
};

// --- Expand (E-expand: add disk(s) to an existing group) --------------------

/**
 * Candidate filtering, first pass: a disk already claimed by ANY
 * mdadm array (`disk.arrays.length > 0`) can never be an expansion target --
 * this is derivable straight from `status --json`'s own inventory, with no
 * backend round-trip needed, so the disk picker can filter it out
 * immediately. This is NOT the authoritative safety check: whether a
 * candidate is a system disk, or carries existing content, is decided
 * exclusively by `shr-rs preflight --json` (see `WritePreflight` in
 * `createGroup.ts`) once the operator has picked a tentative disk -- this
 * function never re-derives that judgment itself.
 */
export const filterExpandCandidates = (disks: DiskStatus[]): DiskStatus[] => (
    disks.filter(disk => disk.arrays.length === 0)
);

export interface ExpandInput {
    groupName: string;
    diskIds: string[];
    forceContent: boolean;
    priority: ReshapePriority;
    /** `shr-rs expand --skip-scrub-check`: waives the engine's requirement
     * that every band of the target group has a scrub that COMPLETED within
     * the last 30 days. Always starts `false`; only
     * `ExpandController.acceptScrubCheckRisk()` turns it on, and only after
     * the engine itself has reported the check failing. */
    skipScrubCheck: boolean;
}

export const expandPreflightArgs = (input: ExpandInput): SpawnCall => (
    preflightArgs(requireNonEmptyDiskList(input.diskIds, _("actions.error.expandNoDisks")), input.forceContent)
);

export const expandDryRunArgs = (input: ExpandInput): SpawnCall => {
    const name = requireNonEmptyName(input.groupName, _("actions.error.expandNoName"));
    const disks = requireNonEmptyDiskList(input.diskIds, _("actions.error.expandNoDisks"));
    return {
        argv: [
            "shr-rs", "expand",
            "--name", name,
            "--add", disks.join(","),
            "--dry-run", "--json",
            ...(input.forceContent ? ["--force-content"] : []),
            ...(input.skipScrubCheck ? ["--skip-scrub-check"] : []),
            // Only appended for a non-default choice -- "balanced"
            // already matches shr-cli's own default_value, so omitting the
            // flag there keeps this argv byte-for-byte what it was before
            // --priority was exposed here at all.
            ...(input.priority !== "balanced" ? ["--priority", input.priority] : []),
        ],
        options: SPAWN_OPTIONS,
    };
};

/**
 * The real, executing call -- derived from `expandDryRunArgs` by stripping
 * `--dry-run` and appending `--yes` (the operator already confirmed via
 * this file's own typed-name gate, so the CLI's own interactive
 * confirmation would just hang waiting on stdin Cockpit never connects),
 * rather than built independently -- the previewed command and the executed
 * command can never drift apart.
 */
export const expandArgs = (input: ExpandInput): SpawnCall => {
    const preview = expandDryRunArgs(input);
    return { argv: [...preview.argv.filter(arg => arg !== "--dry-run"), "--yes"], options: preview.options };
};

export type ExpandStep =
    "preflight" | "blocked" | "preview" | "scrubWarning" | "confirm" | "executing" | "done" | "error";

export interface ExpandState {
    step: ExpandStep;
    preflight: WritePreflight | null;
    preview: CreatePreview | null;
    confirmationText: string;
    result: CreatedGroupSummary | null;
    errorMessage: string | null;
    /** The engine's own scrub-freshness text, kept verbatim for the
     * `scrubWarning` step to show. Null on every other step. */
    scrubCheckWarning: string | null;
}

export const initialExpandState = (): ExpandState => ({
    step: "preflight",
    preflight: null,
    preview: null,
    confirmationText: "",
    result: null,
    errorMessage: null,
    scrubCheckWarning: null,
});

/**
 * Whether a failed expand is the pre-reshape scrub-freshness refusal
 * specifically, as opposed to any other validation error (a degraded band, a
 * reshape already running) that waiving the check would do nothing about.
 *
 * Matched on the `--skip-scrub-check` substring, the same way `shr-tui`'s
 * `is_scrub_check_warning` does it -- that flag name is what the engine's
 * message tells the operator to pass, so its presence IS the signal that an
 * override exists for this refusal. Survives both error shapes Cockpit can
 * receive: the plain stderr line and `--json`'s `{"error": "..."}` envelope.
 */
export const isScrubCheckWarning = (message: string): boolean => message.includes("--skip-scrub-check");

/** Drives one expand run through preflight -> dry-run preview -> typed
 * confirmation -> execute, mirroring `CreateGroupController` in
 * `createGroup.ts`. */
export class ExpandController {
    state: ExpandState = initialExpandState();
    private readonly spawn: Spawn;
    private input: ExpandInput;

    constructor(spawn: Spawn, input: ExpandInput) {
        this.spawn = spawn;
        this.input = input;
    }

    /** Whether this run is going ahead without a fresh scrub, for the UI to
     * repeat at the final confirmation. Read from the input the argv builders
     * use, so it can never disagree with the command actually spawned. */
    get scrubCheckSkipped(): boolean {
        return this.input.skipScrubCheck;
    }

    async runPreflight(): Promise<ExpandState> {
        const { argv, options } = expandPreflightArgs(this.input);
        try {
            const raw = await this.spawn(argv, options);
            const preflight = parseWritePreflight(raw);
            this.state = { ...this.state, step: preflight.ok ? "preview" : "blocked", preflight };
        } catch (error) {
            this.state = { ...this.state, step: "error", errorMessage: spawnErrorMessage(error) };
        }
        return this.state;
    }

    async runPreview(): Promise<ExpandState> {
        if (!this.state.preflight?.ok)
            throw new Error("preview requested before preflight passed -- this is a caller bug, not a user error");
        const { argv, options } = expandDryRunArgs(this.input);
        try {
            const raw = await this.spawn(argv, options);
            const preview = parseCreatePreview(raw);
            this.state = { ...this.state, step: "confirm", preview, scrubCheckWarning: null };
        } catch (error) {
            const message = spawnErrorMessage(error);
            // The dry run is where the engine's scrub-freshness gate fires,
            // and Cockpit used to dead-end on it: the operator saw the raw
            // refusal on the error panel with no way past it, making this the
            // only surface with no equivalent of `--skip-scrub-check`
            // (shr-cli has the flag, shr-tui has `Step::ScrubCheckWarning`).
            // A brand-new group has no scrub history at all, so the very
            // first expand after a create always landed here.
            //
            // `!skipScrubCheck` guards the branch: once the override is on,
            // a message still naming the flag is a different refusal, and
            // offering the same override again would just loop.
            if (!this.input.skipScrubCheck && isScrubCheckWarning(message))
                this.state = { ...this.state, step: "scrubWarning", scrubCheckWarning: message };
            else
                this.state = { ...this.state, step: "error", errorMessage: message };
        }
        return this.state;
    }

    /**
     * Waive the scrub-freshness requirement for this run, the Cockpit
     * equivalent of `--skip-scrub-check`. Callable only from `scrubWarning`,
     * so the bypass cannot be armed ahead of time: the operator can only
     * reach it after the engine has said which band is stale and why it
     * matters. The caller re-runs `runPreview()` to proceed.
     */
    acceptScrubCheckRisk(): ExpandState {
        if (this.state.step !== "scrubWarning")
            throw new Error("acceptScrubCheckRisk() called outside the scrub warning step -- this is a caller bug");
        this.input = { ...this.input, skipScrubCheck: true };
        return this.state;
    }

    setConfirmationText(text: string): ExpandState {
        this.state = { ...this.state, confirmationText: text };
        return this.state;
    }

    canExecute(): boolean {
        return this.state.step === "confirm" &&
            this.state.preview !== null &&
            isConfirmationValid(this.state.confirmationText, this.input.groupName.trim());
    }

    async execute(): Promise<ExpandState> {
        if (!this.canExecute())
            throw new Error("execute() called without a completed preview and a matching typed confirmation");
        this.state = { ...this.state, step: "executing" };
        const { argv, options } = expandArgs(this.input);
        try {
            const raw = await this.spawn(argv, options);
            const result = parseCreatedGroup(raw);
            this.state = { ...this.state, step: "done", result };
        } catch (error) {
            this.state = { ...this.state, step: "error", errorMessage: spawnErrorMessage(error) };
        }
        return this.state;
    }
}

// --- Disk replace (replace one member disk with an equal-or-larger one)

/**
 * `GroupStatus.disks` only carries stable `/dev/disk/by-id` strings; the
 * current `DiskStatus` shape (from `status --json`) has no matching `id`
 * field yet for the UI to correlate against -- what it DOES share with a
 * group is mdadm array membership (`DiskStatus.arrays` lists md device names
 * "this disk backs"; `GroupStatus.bands[].md_name` is that same md device
 * name). Matching on that, rather than waiting on an `id` field neither this
 * file nor `model.ts` owns yet, is what lets the disk-replace dialog show a
 * real kernel name and size for "which physical disk is currently member
 * `old`" without inventing a second identity scheme.
 */
export const groupMemberDisks = (group: GroupStatus, disks: DiskStatus[]): DiskStatus[] => {
    const mdNames = new Set(group.bands.map(band => band.md_name));
    return disks.filter(disk => disk.arrays.some(array => mdNames.has(array)));
};

/**
 * Client-side mirror of the engine's own "equal-or-larger only" rule
 * (`disk replace` is rejected by the engine otherwise) -- filtering here is
 * a UX convenience (don't even list a too-small disk as a candidate), NOT a
 * substitute for the engine's own check, and `replaceArgs` below
 * re-validates the exact same rule immediately before building argv so a
 * caller that bypasses the picker still can't reach spawn with bad input.
 *
 * `candidate.name !== oldDisk.name` only excludes "the same physical disk
 * from itself" -- kernel names are unique within one `status --json` read,
 * so this stays a plain name comparison on purpose (noted in an earlier audit): it is not
 * the identifier that gets sent to the backend, see `ReplaceInput`/
 * `buildReplaceInput` for that.
 *
 * Deliberately does NOT filter on `hasStableId` or `system_disk` -- same
 * split as `filterExpandCandidates` (see its doc comment): this is the
 * "physically plausible" candidate pool (right size, not already claimed by
 * an array), and both of those judgments are surfaced instead as a marked,
 * disabled option in `actionsDialogs.tsx`'s `ReplaceDialog` so the
 * operator sees WHY a same-or-larger free disk can't be picked, rather than
 * it silently vanishing from the list.
 */
export const isValidReplacement = (oldDisk: DiskStatus, candidate: DiskStatus): boolean => (
    candidate.name !== oldDisk.name &&
    candidate.arrays.length === 0 &&
    candidate.size !== null &&
    oldDisk.size !== null &&
    candidate.size >= oldDisk.size
);

export const filterReplacementCandidates = (oldDisk: DiskStatus, disks: DiskStatus[]): DiskStatus[] => (
    disks.filter(candidate => isValidReplacement(oldDisk, candidate))
);

/**
 * Whether `disk` has a genuine, non-empty `/dev/disk/by-id` name.
 * `disk replace` has nothing else to match a disk against -- `--old`
 * matches literally against `StateDisk::id` (`crates/shr-orchestrate/src/
 * engine.rs`'s `replace_disk`), and even `--new` (which `shr-cli`'s own
 * `--help` describes as accepting "by-id, kernel name, or serial") resolves
 * through `resolve_disk_ref`, which itself requires `ByIdIndex::id_for_kernel`
 * to succeed regardless of how the disk was referenced
 * (`IdentityError::NoStableId` otherwise; `crates/shr-inspect/src/diskref.rs`).
 * So a disk failing this predicate can never be a replace source OR target,
 * no matter which of its other identifiers a caller sends.
 */
export const hasStableId = (disk: DiskStatus): boolean => (
    typeof disk.id === "string" && disk.id.length > 0
);

export interface ReplaceInput {
    groupName: string;
    oldId: string;
    newId: string;
    oldSize: number | null;
    newSize: number | null;
}

/**
 * The one place that decides which `DiskStatus` field the engine will
 * actually match a chosen old/new disk against -- `oldId`/`newId` are
 * always `.id` (by-id), never `.name` (kernel name), per `hasStableId`'s
 * doc comment above.
 *
 * Before this function existed, `actionsDialogs.tsx`'s
 * `ReplaceDialog.proceed()` built this object inline using `.name`, which
 * is why Cockpit's disk replace never once matched a real group -- every
 * attempt spawned `--old <kernel-name>` against an engine that only knows
 * the by-id name recorded in state.toml, and always failed with `disk
 * \`<kernel-name>\` is not a member of group \`<name>\``. Pulling this out
 * as a pure function (matching this file's own stated split: "every
 * safety-relevant decision... lives in actions.ts") means the identifier
 * choice is pinned by a test that never has to drive `ReplaceDialog`'s own
 * React state machine.
 *
 * Returns `""` (never falls back to `.name`) for either id when the
 * corresponding disk is `null` or lacks a stable id -- `replaceArgs` below
 * then throws its own clear message rather than silently building a call
 * that could never match the backend anyway.
 */
export const buildReplaceInput = (
    groupName: string, oldDisk: DiskStatus | null, newDisk: DiskStatus | null,
): ReplaceInput => ({
    groupName,
    oldId: oldDisk && hasStableId(oldDisk) ? (oldDisk.id as string) : "",
    newId: newDisk && hasStableId(newDisk) ? (newDisk.id as string) : "",
    oldSize: oldDisk?.size ?? null,
    newSize: newDisk?.size ?? null,
});

export const replaceArgs = (input: ReplaceInput): SpawnCall => {
    const name = requireNonEmptyName(input.groupName, _("actions.error.replaceNoName"));
    const oldId = requireNonEmptyName(input.oldId, _("actions.error.replaceNoOld"));
    const newId = requireNonEmptyName(input.newId, _("actions.error.replaceNoNew"));
    if (oldId === newId)
        throw new Error(_("actions.error.replaceSameDisk"));
    if (input.oldSize === null || input.newSize === null)
        throw new Error(_("actions.error.replaceSizeUnknown"));
    if (input.newSize < input.oldSize)
        throw new Error(_("actions.error.replaceTooSmall"));
    return {
        argv: ["shr-rs", "disk", "replace", "--name", name, "--old", oldId, "--new", newId, "--yes"],
        options: SPAWN_OPTIONS,
    };
};

// --- Recompress (change the Btrfs compress= mount option) -------------

const COMPRESSION_PATTERN = /^[A-Za-z0-9_-]+(:\d+)?$/;

export interface RecompressInput {
    groupName: string;
    compression: string;
}

export const recompressArgs = (input: RecompressInput): SpawnCall => {
    const name = requireNonEmptyName(input.groupName, _("actions.error.recompressNoName"));
    const compression = input.compression.trim();
    if (!COMPRESSION_PATTERN.test(compression))
        throw new Error(_("actions.error.recompressFormat"));
    return {
        argv: ["shr-rs", "fs", "recompress", "--name", name, "--compression", compression],
        options: SPAWN_OPTIONS,
    };
};

// --- Snapshot create ---------------------------------------------------

export interface SnapshotInput {
    groupName: string;
    snapshotName: string;
}

export const snapshotCreateArgs = (input: SnapshotInput): SpawnCall => {
    const group = requireNonEmptyName(input.groupName, _("actions.error.snapshotNoGroup"));
    const name = requireNonEmptyName(input.snapshotName, _("actions.error.snapshotNoName"));
    if (name.includes("/"))
        throw new Error(_("actions.error.snapshotSlash"));
    return {
        argv: ["shr-rs", "fs", "snapshot", "create", name, "--group", group],
        options: SPAWN_OPTIONS,
    };
};

// --- Schedule install (scrub/health-check systemd timers) --------------

// `scrubPriority` sets what the SCHEDULED check runs at. Omitted (the
// default) passes no flag, so `policy.toml`'s `[scrub] priority` decides,
// and with that unset too the scheduled check changes no kernel parameter --
// exactly what every scheduled scrub did before the flag existed.
//
// The generated unit file is where the choice lives: `policy.toml` is
// operator-authored and shr-rs never writes it, so a later `schedule
// install` without the flag goes back to whatever that file says.
export const scheduleInstallArgs = (scrubPriority?: ReshapePriority): SpawnCall => ({
    argv: [
        "shr-rs",
        "schedule",
        "install",
        ...(scrubPriority ? ["--scrub-priority", scrubPriority] : []),
    ],
    options: SPAWN_OPTIONS,
});

// --- Reconcile (finish a deferred resize) -------------------------------
//
// `shr-rs reconcile` finishes any LVM/Btrfs resize a previous `expand` had
// to defer while its mdadm reshape was still running -- the documented,
// verified remedy for the `resize_pending` warning both frontends already
// render (`GroupStatus.resize_pending`/`BandStatus.resize_pending`), which
// neither frontend could act on before this. Unlike every other action in
// this file it is not scoped to one group -- `shr-cli`'s `Command::Reconcile`
// takes no `--name` at all, so `reconcileArgs` takes no input and always
// builds the same bare argv (same shape as `scheduleInstallArgs`).
//
// `OrchestrationEngine::reconcile`'s own doc comment is explicit that it
// never starts a new destructive action -- it only finishes bookkeeping for
// a reshape a prior `expand()` already approved and already physically
// committed -- so it gets `SimpleActionController`, not `TypedConfirmController`:
// one explicit confirm, no typed group-name gate.
export const reconcileArgs = (): SpawnCall => ({
    argv: ["shr-rs", "reconcile"],
    options: SPAWN_OPTIONS,
});

// --- Destroy (tear down a group entirely -- the only CLI-only op) -----------
//
// `shr-rs destroy` unmounts, removes the LV/VG/PVs, stops every mdadm array,
// drops the group from state.toml, and regenerates mdadm.conf/fstab -- every
// other CLI operation (create/expand/replace/scrub/recompress/snapshot/
// schedule/reconcile) already has a Cockpit path, but destroy had none, so a
// GUI-only operator had no correct way to remove a group (shr-cli's own
// --help warns a hand-teardown "leaves orphaned managed-block entries
// behind"). Gets the same shape as `replaceArgs`/`recompressArgs`
// (TypedConfirmController, typed group-name confirmation) since it is at
// least as destructive as either.

export interface DestroyInput {
    groupName: string;
    zeroSuperblocks: boolean;
}

export interface DestroyResult {
    destroyed: string;
}

/** `--yes` is always present: the UI's own `TypedConfirmController` typed-name
 * gate is what did the actual confirming, so `shr-cli`'s own interactive
 * confirmation would just hang waiting on stdin Cockpit never connects (same
 * reasoning as `expandArgs`'s doc comment). `--zero-superblocks` only appears
 * when explicitly requested -- off by default keeps member partitions'
 * mdadm superblocks intact (recoverable via `mdadm --assemble --scan`); on
 * makes them unrecoverable, so it must never be silently implied. */
export const destroyArgs = (input: DestroyInput): SpawnCall => {
    const name = requireNonEmptyName(input.groupName, _("actions.error.destroyNoName"));
    return {
        argv: [
            "shr-rs", "destroy",
            "--name", name,
            "--yes", "--json",
            // Always one flag or the other, never "omit it and take the
            // default": `--yes` makes this a non-interactive run, and
            // `destroy` now refuses to pick the superblock decision for a
            // caller that never states one. The checkbox IS the operator's
            // answer, so both of its positions have to be spelled out.
            input.zeroSuperblocks ? "--zero-superblocks" : "--no-zero-superblocks",
        ],
        options: SPAWN_OPTIONS,
    };
};

/** Parses `shr-rs destroy --json`'s stdout -- a single `{"destroyed":
 * "<group>"}` object (`Command::Destroy`'s json branch, `crates/shr-cli/src/
 * lib.rs`). Throws on a shape mismatch rather than defaulting past it, same
 * discipline as every other parser in this file. */
export const parseDestroyResult = (raw: string): DestroyResult => {
    let value: unknown;
    try {
        value = JSON.parse(raw);
    } catch {
        throw new Error(_("actions.error.destroyNotJson"));
    }
    const report = requireRecord(value, _("actions.error.destroyNotObject"));
    return { destroyed: requireString(report.destroyed, _("actions.error.destroyDestroyed")) };
};

// --- Shared confirm-gated controllers ---------------------------------------
//
// Two shapes cover every remaining action:
//  - `TypedConfirmController`: destructive, no CLI dry-run exists (disk
//    replace, recompress, destroy) -- the operator must type the exact
//    group name, same bar as `ExpandController`/`CreateGroupController`.
//  - `SimpleActionController`: not destructive to existing data (scrub
//    start/cancel, snapshot create, schedule install, reconcile) -- a
//    single explicit confirm() click gates the spawn, no typed name
//    required.
// Both refuse to have ever spawned anything before their gate is satisfied
// -- `execute()` throws rather than silently no-op'ing, so a caller bypassing
// `canExecute()` is a bug in the wiring, not a state this must paper over.

export type ConfirmStep = "review" | "confirm" | "executing" | "done" | "error";

export interface TypedConfirmState<T> {
    step: ConfirmStep;
    confirmationText: string;
    result: T | null;
    errorMessage: string | null;
}

export class TypedConfirmController<TInput, TResult> {
    state: TypedConfirmState<TResult> = { step: "review", confirmationText: "", result: null, errorMessage: null };
    private readonly spawn: Spawn;
    private readonly input: TInput;
    private readonly buildCall: (input: TInput) => SpawnCall;
    private readonly expectedName: (input: TInput) => string;
    private readonly parseResult: (raw: string) => TResult;

    constructor(
        spawn: Spawn,
        input: TInput,
        buildCall: (input: TInput) => SpawnCall,
        expectedName: (input: TInput) => string,
        parseResult: (raw: string) => TResult,
    ) {
        this.spawn = spawn;
        this.input = input;
        this.buildCall = buildCall;
        this.expectedName = expectedName;
        this.parseResult = parseResult;
    }

    /** Validates the input (throws on e.g. a too-small replacement disk or a
     * malformed compression string -- see `buildCall`) before advancing past
     * review. Never spawns. */
    proceedToConfirm(): TypedConfirmState<TResult> {
        this.buildCall(this.input);
        this.state = { ...this.state, step: "confirm" };
        return this.state;
    }

    setConfirmationText(text: string): TypedConfirmState<TResult> {
        this.state = { ...this.state, confirmationText: text };
        return this.state;
    }

    canExecute(): boolean {
        return this.state.step === "confirm" &&
            isConfirmationValid(this.state.confirmationText, this.expectedName(this.input).trim());
    }

    async execute(): Promise<TypedConfirmState<TResult>> {
        if (!this.canExecute())
            throw new Error("execute() called without a matching typed confirmation");
        this.state = { ...this.state, step: "executing" };
        const { argv, options } = this.buildCall(this.input);
        try {
            const raw = await this.spawn(argv, options);
            const result = this.parseResult(raw);
            this.state = { ...this.state, step: "done", result };
        } catch (error) {
            this.state = { ...this.state, step: "error", errorMessage: spawnErrorMessage(error) };
        }
        return this.state;
    }
}

export interface SimpleActionState<T> {
    step: ConfirmStep;
    confirmed: boolean;
    result: T | null;
    errorMessage: string | null;
}

export class SimpleActionController<TInput, TResult> {
    state: SimpleActionState<TResult> = { step: "review", confirmed: false, result: null, errorMessage: null };
    private readonly spawn: Spawn;
    private readonly input: TInput;
    private readonly buildCall: (input: TInput) => SpawnCall;
    private readonly parseResult: (raw: string) => TResult;

    constructor(spawn: Spawn, input: TInput, buildCall: (input: TInput) => SpawnCall, parseResult: (raw: string) => TResult) {
        this.spawn = spawn;
        this.input = input;
        this.buildCall = buildCall;
        this.parseResult = parseResult;
    }

    /** Validates the input (throws on e.g. an empty group name) before
     * advancing past review. Never spawns. */
    proceedToConfirm(): SimpleActionState<TResult> {
        this.buildCall(this.input);
        this.state = { ...this.state, step: "confirm" };
        return this.state;
    }

    confirm(): SimpleActionState<TResult> {
        this.state = { ...this.state, confirmed: true };
        return this.state;
    }

    canExecute(): boolean {
        return this.state.step === "confirm" && this.state.confirmed;
    }

    async execute(): Promise<SimpleActionState<TResult>> {
        if (!this.canExecute())
            throw new Error("execute() called without confirming");
        this.state = { ...this.state, step: "executing" };
        const { argv, options } = this.buildCall(this.input);
        try {
            const raw = await this.spawn(argv, options);
            const result = this.parseResult(raw);
            this.state = { ...this.state, step: "done", result };
        } catch (error) {
            this.state = { ...this.state, step: "error", errorMessage: spawnErrorMessage(error) };
        }
        return this.state;
    }
}
