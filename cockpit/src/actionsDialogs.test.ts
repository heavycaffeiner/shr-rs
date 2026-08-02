// SPDX-License-Identifier: LGPL-2.1-or-later
//
// Every operational dialog (`actionsDialogs.tsx`) must refuse to close
// while its destructive command is in flight. Before this file, that claim
// had NO automated test at all -- the design called it
// "unit-tested", but grepping the tree found no test file touching
// `actionsDialogs.tsx` or its `closeDisabled`/`busy` wiring anywhere.
//
// `actionsDialogs.tsx` is JSX, and this project's plain `node --test
// src/*.test.ts` has no JSX/TSX loader (verified: Node's own type-stripping
// rejects the `.tsx` extension outright, and no jsdom/testing-library is
// installed). Rather than add new devDependencies for one file, this uses
// the `esbuild` package already a runtime dependency here (it drives
// `build.js`) to transform the real source on the fly, then renders it with
// `react-dom/server` -- no jsdom, but real React reading the real disabled
// attribute off the real component, not a hand-rolled fake of it.
//
// The technique above (`renderToStaticMarkup`) is static -- nothing
// can click a button or observe a subsequent unmount, so it cannot reproduce
// "a done panel that sets, then gets unmounted before it paints". That
// needed genuine reconciliation: `react-test-renderer` (added as a
// devDependency for this fix, version-pinned to this project's existing
// react/react-dom 18.3.1) gives real hooks/effects/unmounting with no DOM.
// See `makeAppShapedHarness` below for how it's used without reimplementing
// any dialog's own logic.
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import * as esbuild from "esbuild";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { act, create } from "react-test-renderer";

import type { DestroyInput, ReplaceInput, SimpleActionState, TypedConfirmState } from "./actions.ts";
import type { DiskStatus, GroupStatus } from "./model.ts";
import { installEnglishCatalog } from "./testCatalog.ts";
import type {
    ReactTestInstance,
    ReactTestRenderer,
    ReactTestRendererJSON,
    ReactTestRendererNode,
} from "react-test-renderer";

// The msgids in `src/` are dotted keys, so without a catalogue every string
// below would render as its key. See testCatalog.ts.
installEnglishCatalog();

// `cockpit.ts` throws at module load if `window.cockpit` is missing --
// `actionsDialogs.tsx` imports it statically even though the one component
// under test here (`Modal`) never calls it. Stubbed only for this test
// process; production `cockpit.ts` is untouched.
(globalThis as unknown as { window: unknown }).window = { cockpit: { spawn: async () => "" } };

// PatternFly's `Alert` registers a capture-phase focus listener in an
// unguarded `useEffect` (Alert.js:64) -- no `typeof document !== "undefined"`
// check. `react-test-renderer` commits effects (unlike the
// `renderToStaticMarkup` path most of this suite uses), so the three
// interactive tests (Reconcile/Schedule/Destroy) need `document` to exist at
// all. `divRef.current` is always null under `react-test-renderer` (it never
// attaches to a real DOM node), so the listener's body -- which only reads
// `document.activeElement` after checking `divRef.current` -- never actually
// dereferences anything on this stub; these three no-ops are sufficient for
// mount and unmount both.
//
// Deliberately on `globalThis`, NOT merged into the `window` stub above:
// PatternFly's `canUseDOM` is `!!(typeof window !== "undefined" &&
// window.document && window.document.createElement)` (helpers/util.js:337).
// Leaving `window.document` unset keeps `canUseDOM` false, which is what
// keeps every portal-based PatternFly component (including the real
// `Modal`, which this file relies on rendering as an empty string under
// `renderToStaticMarkup`) behaving the same as before this stub existed.
// Do not "tidy" this onto `window.document` -- that would flip `canUseDOM`
// and silently change other components' behavior.
(globalThis as unknown as { document: unknown }).document = {
    addEventListener() {},
    removeEventListener() {},
    activeElement: null,
};

const srcDir = path.dirname(fileURLToPath(import.meta.url));
const actionsDialogsPath = path.join(srcDir, "actionsDialogs.tsx");

/** Bundles the real `actionsDialogs.tsx` (react/react-dom left external so
 * this process's own React instance is the one that runs) and loads it as a
 * throwaway ESM file. */
const loadActionsDialogsModule = async (): Promise<{
    Modal: React.ComponentType<{
        title: string; onClose: () => void; children?: React.ReactNode; closeDisabled?: boolean;
    }>;
    ExpandDialog: React.ComponentType<{
        group: GroupStatus; disks: DiskStatus[]; onClose: () => void; onChanged: () => void;
    }>;
    ReplaceDialog: React.ComponentType<{
        group: GroupStatus; disks: DiskStatus[]; onClose: () => void; onChanged: () => void;
    }>;
    ReplaceConfirmStep: React.ComponentType<{
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
    }>;
    RecompressConfirmStep: React.ComponentType<{
        group: GroupStatus;
        compression: string;
        confirmText: string;
        onConfirmText: (text: string) => void;
        state: TypedConfirmState<string>;
        busy: boolean;
        canExecute: boolean;
        onCancel: () => void;
        onExecute: () => void;
    }>;
    SnapshotConfirmStep: React.ComponentType<{
        group: GroupStatus;
        snapshotName: string;
        state: SimpleActionState<string>;
        busy: boolean;
        onCancel: () => void;
        onExecute: () => void;
    }>;
    DestroyDialog: React.ComponentType<{
        group: GroupStatus; onClose: () => void; onChanged: () => void;
    }>;
    DestroyConfirmStep: React.ComponentType<{
        group: GroupStatus;
        destroyInput: DestroyInput;
        confirmText: string;
        onConfirmText: (text: string) => void;
        state: TypedConfirmState<{ destroyed: string }>;
        busy: boolean;
        canExecute: boolean;
        onCancel: () => void;
        onExecute: () => void;
    }>;
    OperationsPanel: React.ComponentType<{
        groups: GroupStatus[]; disks: DiskStatus[]; onChanged: () => void;
    }>;
}> => {
    const result = await esbuild.build({
        entryPoints: [actionsDialogsPath],
        bundle: true,
        format: "esm",
        platform: "node",
        write: false,
        external: ["react", "react-dom", "react-dom/*"],
        // PatternFly's components pull their stylesheet in as a side effect
        // (`react-styles/css/.../accordion.js` does `import './accordion.css'`).
        // Dropping the CSS is safe and necessary here: the class names these
        // assertions see come from the JS module's exports, not the
        // stylesheet, and with `write: false` esbuild has no output path to
        // put a CSS file at -- it errors out rather than emitting one.
        loader: { ".css": "empty" },
        // PatternFly ships no `exports` map, so `mainFields` picks the build:
        // `platform: "node"` would default to `["main"]` and pull in the CJS
        // tree, whose `require("react")` cannot be satisfied once React is
        // external and the output format is ESM. This is also the build the
        // production bundle resolves (build.js runs at the default browser
        // platform), so test and shipped plugin exercise the same PatternFly code.
        mainFields: ["module", "main"],
    });
    const code = result.outputFiles[0].text;
    // Written inside `src/` (not the OS tmpdir) so bare specifiers like
    // "react" resolve through this project's own node_modules -- Node
    // resolves those by walking up from the importing file's location.
    // Deleted in the `finally` below either way.
    const tmpFile = path.join(srcDir, `_actionsDialogs.esbuild-tmp.${process.pid}.mjs`);
    fs.writeFileSync(tmpFile, code);
    try {
        return await import(`file://${tmpFile.split("\\").join("/")}`) as never;
    } finally {
        fs.rmSync(tmpFile, { force: true });
    }
};

test("Modal's close button is a real disabled <button> while closeDisabled, with the in-flight reason as its title", async () => {
    const { Modal } = await loadActionsDialogsModule();
    const html = renderToStaticMarkup(
        React.createElement(Modal, { title: "Expand group \"g1\"", onClose: () => {}, closeDisabled: true }, "body"),
    );
    // Re-anchored off the close button's `aria-label` rather than `class="modal-close"`:
    // the PatternFly conversion hand-renders this button (see Modal's own
    // doc comment for why) without that class name, but the `aria-label`
    // is still the one attribute the contract actually requires.
    const closeButton = html.match(/<button[^>]*aria-label="Close"[^>]*>/)?.[0];
    assert.ok(closeButton, `no close button found in: ${html}`);
    // The real invariant: a `disabled` HTML attribute is what makes a
    // button non-interactive in a real browser, independent of whatever
    // this test can or can't simulate clicking -- asserting on it is
    // asserting on the actual close affordance, not merely that some prop
    // reached the component.
    assert.match(closeButton, /\bdisabled=""/, "close button must carry the disabled attribute while busy");
    assert.match(closeButton, /title="An operation is in progress\./);
});

test("Modal's close button is enabled with no in-flight title when not closeDisabled", async () => {
    const { Modal } = await loadActionsDialogsModule();
    const html = renderToStaticMarkup(
        React.createElement(Modal, { title: "Expand group \"g1\"", onClose: () => {}, closeDisabled: false }, "body"),
    );
    // Re-anchored off the close button's `aria-label`, same reason as the test above.
    const closeButton = html.match(/<button[^>]*aria-label="Close"[^>]*>/)?.[0];
    assert.ok(closeButton, `no close button found in: ${html}`);
    assert.doesNotMatch(closeButton, /\bdisabled/, "close button must not be disabled when nothing is in flight");
    assert.doesNotMatch(closeButton, /title=/, "no in-flight tooltip when nothing is in flight");
});

// Every dialog in this file routes through this one `Modal`, so its box
// class is the single place the plugin's dialog geometry is decided. A bare
// `pf-v6-c-modal-box` is not "default sized": modal-box.css gives it
// `--Width: 100%` and `--MaxWidth: calc(100% - spacer--xl)`, so it stretches
// the frame. Measured live before the fix, a two-button confirmation dialog
// was 1417px wide in a 1449px frame. Stock Cockpit never does this -- `/users`
// and `/sosreport`, from two unrelated Cockpit packages, both render
// `pf-v6-c-modal-box pf-m-align-top pf-m-md` (840px, 305px gutters, same
// browser, same frame width). `pf-m-align-top` is PatternFly's recommended
// placement for modals with expanding content, which these are: each grows as
// its command preview expands and its result alert appears.
//
// Anchors on the box's own class attribute, not a document-wide substring
// search: `pf-m-md` and `pf-m-align-top` are generic PatternFly modifiers
// carried by many components, so searching the whole render would keep passing
// if they ever landed on something nested inside the dialog instead.
test("Modal's box carries the same size and placement modifiers stock Cockpit's dialogs use", async () => {
    const { Modal } = await loadActionsDialogsModule();
    const html = renderToStaticMarkup(
        React.createElement(Modal, { title: "Expand group \"g1\"", onClose: () => {} }, "body"),
    );

    const match = /<div class="(pf-v6-c-modal-box[^"]*)"/.exec(html);
    // Guards the guard: without this, a restructured chrome that no longer
    // renders the box as a `div` with a leading `class` would leave the loop
    // below with nothing to iterate over and pass vacuously.
    assert.ok(match, `found no <div class="pf-v6-c-modal-box..."> in: ${html}`);

    // The consequence text is per-modifier rather than one shared sentence:
    // saying "the box goes full-bleed" when what's actually missing is the
    // placement modifier would describe something adjacent to, but not the
    // same as, what the assertion checked.
    const consequences: Record<string, string> = {
        "pf-m-md": "without a size modifier the box takes modal-box.css's --Width: 100% and goes full-bleed",
        "pf-m-align-top": "without it the box is vertically centred, so it jumps as its content expands",
    };
    const boxClass = match[1];
    for (const [modifier, consequence] of Object.entries(consequences)) {
        assert.ok(
            boxClass.split(" ").includes(modifier),
            `Modal's box is missing ${modifier}; stock Cockpit renders ` +
            `"pf-v6-c-modal-box pf-m-align-top pf-m-md", this renders "${boxClass}" -- ${consequence}.`,
        );
    }
});

// Answers the brief's own question: that fix touched six dialogs (scrub,
// expand, replace, recompress, snapshot, schedule), not just the one or two
// that would come up first when reproducing the bug by hand. A future
// dialog that forgets to wire its own `busy` into `<Modal closeDisabled>`
// would silently reopen this exact defect for just that one dialog --
// counting every `<Modal ... closeDisabled={busy}>` call site is what
// catches that, cheaply, without needing to drive each dialog's async flow.
// Bumped to 8 when `DestroyDialog` was added -- same gating is required of
// it as every other dialog in this file.
test("every one of the eight dialogs gates its own Modal on its own busy state, not just the two named in the original brief", () => {
    const source = fs.readFileSync(actionsDialogsPath, "utf8");
    const gated = source.match(/<Modal\b[^>]*\bcloseDisabled=\{busy\}/g) ?? [];
    assert.equal(
        gated.length,
        8,
        `expected closeDisabled={busy} on all 8 dialogs' <Modal> (scrub/expand/replace/recompress/snapshot/destroy/schedule/reconcile), found ${gated.length}: ${JSON.stringify(gated)}`,
    );
});

// The expand dialog's disk picker listed a system disk (`vda`, holding
// `/`/`/boot`/`/boot/efi`) as a plain, freely-checkable checkbox -- visually
// identical to a genuinely free disk. `filterExpandCandidates` deliberately
// does not filter system disks out (see its doc comment in actions.ts --
// `preflight --json` alone is authoritative for actually blocking one), so
// an operator only learned they picked the OS disk one step later, at the
// "blocked" preflight screen. Mirrors createGroupWizard.tsx's identical
// fix: mark + disable using the `status --json`-sourced `disk.system_disk`
// field the dashboard's own drive table already renders a warning from.
const expandGroupFixture = (): GroupStatus => ({
    name: "shr1",
    mode: "shr",
    layout_version: 1,
    mount_point: "/mnt/shr_data",
    fs_uuid: "abc-123",
    usable_bytes: 8_000_000_000_000,
    resize_pending: false,
    disks: [],
    bands: [],
});

const expandDiskFixture = (overrides: Partial<DiskStatus> = {}): DiskStatus => ({
    name: "sdc",
    size: 4_000_000_000_000,
    model: "Model X",
    serial: "SERIAL1",
    rotational: true,
    smart: {
        state: "ok",
        temperature_c: null,
        power_on_hours: null,
        pending_sectors: null,
        reallocated_sectors: null,
        uncorrectable_sectors: null,
        nvme_critical_warning: null,
    },
    arrays: [],
    ...overrides,
});

test("ExpandDialog's disk picker disables and labels a system-disk candidate, leaving a plain disk freely selectable", async () => {
    const { ExpandDialog } = await loadActionsDialogsModule();
    const disks: DiskStatus[] = [
        expandDiskFixture({ name: "vda", system_disk: true, system_mounts: ["/", "/boot", "/boot/efi"] }),
        expandDiskFixture({ name: "sdc", system_disk: false }),
    ];
    const html = renderToStaticMarkup(
        React.createElement(ExpandDialog, {
            group: expandGroupFixture(), disks, onClose: () => {}, onChanged: () => {},
        }),
    );

    // Rows are PatternFly checkboxes (`pf-v6-c-check`) since the hand-rolled
    // `app.scss` was dropped, so every row -- and the dialog's own
    // force-content checkbox -- shares one label class. Select rows by the
    // disk they name, then assert on the state each one carries; that also
    // stops this test from presupposing which class the picker chose.
    const rows = [...html.matchAll(/<label class="pf-v6-c-check"[^]*?<\/label>/g)].map(m => m[0]);

    // The system disk's row: disabled input, visibly marked, same wording
    // family as createGroupWizard.tsx's picker and panels.tsx's drive table.
    const vdaRow = rows.find(row => row.includes("vda"));
    assert.ok(vdaRow, `no row rendered for the system disk in: ${html}`);
    assert.match(vdaRow, /<span class="pf-v6-c-check__label pf-m-disabled"/, "the system disk's row must be visibly marked disabled, not just inert");
    assert.match(vdaRow, /<input[^>]*\bdisabled=""/, "the system disk's checkbox itself must carry the disabled attribute");
    assert.match(vdaRow, /System disk \(cannot be selected\)/, "must warn the operator inline, not just disable silently");
    assert.match(vdaRow, /\/, \/boot, \/boot\/efi/, "must surface which mounts make it a system disk");

    // A non-system candidate must render unmarked and freely selectable --
    // this fix must not disable every candidate.
    const sdcRow = rows.find(row => row.includes("sdc"));
    assert.ok(sdcRow, `no row rendered for sdc in: ${html}`);
    assert.doesNotMatch(sdcRow, /pf-m-disabled/, "a non-system candidate must not be marked disabled");
    assert.doesNotMatch(sdcRow, /\bdisabled=""/, "a non-system candidate's checkbox must stay enabled");
});

// `resize_pending` was rendered as a warning badge (dashboard's group/
// band/fs tables, in `panels.tsx`) with no way to act on it from either
// frontend -- `shr-rs reconcile` is the documented fix. `OperationsPanel` is
// the surface this file owns that sits right next to that dashboard, so the
// reconcile trigger must be reachable from here, and must be visibly flagged
// when a group actually has a pending resize (not just present as one more
// unmarked button an operator would have no reason to notice).
const reconcileGroupFixture = (overrides: Partial<GroupStatus> = {}): GroupStatus => ({
    name: "shr1",
    mode: "shr",
    layout_version: 1,
    mount_point: "/mnt/shr_data",
    fs_uuid: "abc-123",
    usable_bytes: 8_000_000_000_000,
    resize_pending: false,
    disks: [],
    bands: [],
    ...overrides,
});

test("OperationsPanel renders a reconcile action, unflagged when nothing is resize_pending", async () => {
    const { OperationsPanel } = await loadActionsDialogsModule();
    const html = renderToStaticMarkup(
        React.createElement(OperationsPanel, {
            groups: [reconcileGroupFixture({ resize_pending: false })], disks: [], onChanged: () => {},
        }),
    );
    assert.match(html, /Finish the expansion/, "must render a reconcile trigger somewhere in the panel");
    assert.doesNotMatch(html, /still waiting for an expansion/, "no pending-resize warning when nothing is pending");
});

test("OperationsPanel flags the reconcile action when a group has resize_pending", async () => {
    const { OperationsPanel } = await loadActionsDialogsModule();
    const html = renderToStaticMarkup(
        React.createElement(OperationsPanel, {
            groups: [reconcileGroupFixture({ name: "shr1", resize_pending: true })], disks: [], onChanged: () => {},
        }),
    );
    assert.match(html, /Finish the expansion/);
    assert.match(html, /still waiting for an expansion/, "must surface the pending-resize warning right where the operator can act on it");
    assert.match(html, /shr1/, "must name which group has the pending resize");
});

// Cockpit's disk replace has never once worked -- `ReplaceDialog` sent
// `DiskStatus.name` (kernel name, e.g. "loop10") as both `--old`/`--new`,
// but the engine's `replace_disk` matches `--old` literally against
// `StateDisk::id` (by-id, e.g. "ata-LOOP_DISK_10"). Confirmed against a
// live array: `disk replace --old loop10 ...` fails with `disk 'loop10' is
// not a member of group 'demo1'`, while `--old ata-LOOP_DISK_10 ...`
// succeeds. And when it failed, the dialog rendered as a bare Modal shell
// (heading + close button, no error text at all) -- the worse defect,
// since an operator would conclude the replace succeeded when it hadn't.
const replaceGroupFixture = (): GroupStatus => ({
    name: "demo1",
    mode: "shr",
    layout_version: 1,
    mount_point: "/mnt/shr_data",
    fs_uuid: "abc-123",
    usable_bytes: 8_000_000_000_000,
    resize_pending: false,
    disks: [],
    bands: [{
        index: 0,
        level: "raid5",
        md_name: "md0",
        usable_bytes: 8_000_000_000_000,
        resize_pending: false,
        members: ["loop10"],
        sync: null,
        last_scrub: null,
        scrub_in_progress: false,
    }],
});

const replaceDiskFixture = (overrides: Partial<DiskStatus> = {}): DiskStatus => ({
    name: "sdz",
    size: 4_000_000_000_000,
    model: "Model X",
    serial: "SERIAL1",
    rotational: true,
    smart: {
        state: "ok",
        temperature_c: null,
        power_on_hours: null,
        pending_sectors: null,
        reallocated_sectors: null,
        uncorrectable_sectors: null,
        nvme_critical_warning: null,
    },
    arrays: [],
    ...overrides,
});

test("ReplaceDialog's old-disk picker disables and labels a member disk with no stable id", async () => {
    const { ReplaceDialog } = await loadActionsDialogsModule();
    const disks: DiskStatus[] = [
        replaceDiskFixture({ name: "loop10", id: null, arrays: ["md0"] }),
    ];
    const html = renderToStaticMarkup(
        React.createElement(ReplaceDialog, { group: replaceGroupFixture(), disks, onClose: () => {}, onChanged: () => {} }),
    );
    const option = html.match(/<option value="loop10"[^>]*>[^<]*<\/option>/)?.[0];
    assert.ok(option, `no option rendered for loop10 in: ${html}`);
    assert.match(option, /\bdisabled=""/, "an id-less member disk's option must be disabled, not just visually different");
    assert.match(option, /no stable identifier \(by-id\)/, "must tell the operator WHY this disk can't be picked");
});

test("ReplaceDialog's new-disk picker disables and labels a system-disk candidate, leaving a plain candidate selectable", async () => {
    const { ReplaceDialog } = await loadActionsDialogsModule();
    const disks: DiskStatus[] = [
        replaceDiskFixture({ name: "loop10", id: "ata-LOOP_DISK_10", size: 4_000_000_000_000, arrays: ["md0"] }),
        replaceDiskFixture({ name: "vda", id: "ata-VDA", size: 4_000_000_000_000, system_disk: true, system_mounts: ["/", "/boot"] }),
        replaceDiskFixture({ name: "loop13", id: "ata-LOOP_DISK_13", size: 4_000_000_000_000 }),
    ];
    const html = renderToStaticMarkup(
        React.createElement(ReplaceDialog, { group: replaceGroupFixture(), disks, onClose: () => {}, onChanged: () => {} }),
    );

    const vdaOption = html.match(/<option value="vda"[^>]*>[^<]*<\/option>/)?.[0];
    assert.ok(vdaOption, `no option rendered for vda (system disk) in: ${html}`);
    assert.match(vdaOption, /\bdisabled=""/, "the system disk must not be a selectable replacement target");
    assert.match(vdaOption, /system disk/, "must warn the operator inline, not just disable silently");
    assert.match(vdaOption, /\/, \/boot/, "must surface which mounts make it a system disk");

    const loop13Option = html.match(/<option value="loop13"[^>]*>[^<]*<\/option>/)?.[0];
    assert.ok(loop13Option, `no option rendered for loop13 in: ${html}`);
    assert.doesNotMatch(loop13Option, /\bdisabled=""/, "a plain same-or-larger candidate must stay selectable");
});

test("ReplaceDialog's old-disk picker defaults to a disk that actually has a stable id, skipping an id-less first member", async () => {
    const { ReplaceDialog } = await loadActionsDialogsModule();
    const group: GroupStatus = {
        ...replaceGroupFixture(),
        bands: [{
            index: 0,
            level: "raid5",
            md_name: "md0",
            usable_bytes: 8_000_000_000_000,
            resize_pending: false,
            members: ["loop10", "loop11"],
            sync: null,
            last_scrub: null,
            scrub_in_progress: false,
        }],
    };
    const disks: DiskStatus[] = [
        replaceDiskFixture({ name: "loop10", id: null, arrays: ["md0"] }),
        replaceDiskFixture({ name: "loop11", id: "ata-LOOP_DISK_11", arrays: ["md0"] }),
    ];
    const html = renderToStaticMarkup(
        React.createElement(ReplaceDialog, { group, disks, onClose: () => {}, onChanged: () => {} }),
    );
    const selected = html.match(/<option value="loop11"[^>]*selected=""[^>]*>/);
    assert.ok(selected, `expected loop11 (the disk that has a stable id) to be the default selection in: ${html}`);
});

// This is the direct proof for the "silent failure" defect: a real render
// of the exact component `ReplaceDialog` uses for its confirm/error body,
// fed a `state.step === "error"` shaped exactly like what
// `TypedConfirmController.execute()` produces on a real spawn rejection,
// asserting the backend's own message text is actually present in the
// rendered HTML -- not merely that some internal error-state variable was
// set somewhere.
test("ReplaceConfirmStep renders the backend's error message on state.step === \"error\" (not a bare shell)", async () => {
    const { ReplaceConfirmStep } = await loadActionsDialogsModule();
    const replaceInput: ReplaceInput = {
        groupName: "demo1",
        oldId: "ata-LOOP_DISK_10",
        newId: "ata-LOOP_DISK_13",
        oldSize: 4_000_000_000_000,
        newSize: 4_000_000_000_000,
    };
    const errorState: TypedConfirmState<string> = {
        step: "error",
        confirmationText: "demo1",
        result: null,
        errorMessage: "Validation error: disk 'loop10' is not a member of group 'demo1'",
    };
    const html = renderToStaticMarkup(
        React.createElement(ReplaceConfirmStep, {
            group: replaceGroupFixture(),
            oldName: "loop10",
            newName: "loop13",
            replaceInput,
            confirmText: "demo1",
            onConfirmText: () => {},
            state: errorState,
            busy: false,
            canExecute: true,
            onCancel: () => {},
            onExecute: () => {},
        }),
    );
    // react-dom/server HTML-escapes text content, so a literal apostrophe in
    // the backend's message renders as `&#x27;` -- match that, not a raw `'`.
    assert.match(
        html,
        /disk &#x27;loop10&#x27; is not a member of group &#x27;demo1&#x27;/,
        `the backend's own error message must actually appear in the rendered output, got: ${html}`,
    );
});

test("ReplaceConfirmStep's command preview shows the real by-id argv, matching what TypedConfirmController actually spawns", async () => {
    const { ReplaceConfirmStep } = await loadActionsDialogsModule();
    const replaceInput: ReplaceInput = {
        groupName: "demo1",
        oldId: "ata-LOOP_DISK_10",
        newId: "ata-LOOP_DISK_13",
        oldSize: 4_000_000_000_000,
        newSize: 4_000_000_000_000,
    };
    const confirmState: TypedConfirmState<string> = { step: "confirm", confirmationText: "", result: null, errorMessage: null };
    const html = renderToStaticMarkup(
        React.createElement(ReplaceConfirmStep, {
            group: replaceGroupFixture(),
            oldName: "loop10",
            newName: "loop13",
            replaceInput,
            confirmText: "",
            onConfirmText: () => {},
            state: confirmState,
            busy: false,
            canExecute: false,
            onCancel: () => {},
            onExecute: () => {},
        }),
    );
    assert.match(
        html,
        /disk replace --name demo1 --old ata-LOOP_DISK_10 --new ata-LOOP_DISK_13 --yes/,
        `the previewed command must be the real by-id argv, not the kernel names: ${html}`,
    );
    assert.doesNotMatch(html, /--old loop10/, "the preview must not show the kernel name as the identifier that will be sent");
});

// An earlier fix addressed this exact shape (error paragraph nested inside a `step ===
// "confirm"`-only render branch, discarded the instant
// TypedConfirmController.execute()/SimpleActionController.execute() move
// state.step from "confirm" straight to "error") for ReplaceDialog only. The
// same nesting existed verbatim in RecompressDialog and SnapshotDialog --
// found by re-reading every dialog in this file, not by re-reproducing each
// one in a browser. `RecompressConfirmStep`/`SnapshotConfirmStep` are pulled
// out the same way `ReplaceConfirmStep` was, so these tests render the exact
// body each dialog uses for its confirm/error screen and assert the
// backend's own message text is actually present -- not merely that
// `state.step === "error"` was reached.
const recompressGroupFixture = (): GroupStatus => ({
    name: "shr1",
    mode: "shr",
    layout_version: 1,
    mount_point: "/mnt/shr_data",
    fs_uuid: "abc-123",
    usable_bytes: 8_000_000_000_000,
    resize_pending: false,
    disks: [],
    bands: [],
});

test("RecompressConfirmStep renders the backend's error message on state.step === \"error\" (not a bare shell)", async () => {
    const { RecompressConfirmStep } = await loadActionsDialogsModule();
    const errorState: TypedConfirmState<string> = {
        step: "error",
        confirmationText: "shr1",
        result: null,
        errorMessage: "Validation error: band 0 (md0) has background activity in progress",
    };
    const html = renderToStaticMarkup(
        React.createElement(RecompressConfirmStep, {
            group: recompressGroupFixture(),
            compression: "zstd:3",
            confirmText: "shr1",
            onConfirmText: () => {},
            state: errorState,
            busy: false,
            canExecute: true,
            onCancel: () => {},
            onExecute: () => {},
        }),
    );
    assert.match(
        html,
        /band 0 \(md0\) has background activity in progress/,
        `the backend's own error message must actually appear in the rendered output, got: ${html}`,
    );
});

test("SnapshotConfirmStep renders the backend's error message on state.step === \"error\" (not a bare shell)", async () => {
    const { SnapshotConfirmStep } = await loadActionsDialogsModule();
    const errorState: SimpleActionState<string> = {
        step: "error",
        confirmed: true,
        result: null,
        errorMessage: "Validation error: snapshot name 'before-upgrade' already exists",
    };
    const html = renderToStaticMarkup(
        React.createElement(SnapshotConfirmStep, {
            group: recompressGroupFixture(),
            snapshotName: "before-upgrade",
            state: errorState,
            busy: false,
            onCancel: () => {},
            onExecute: () => {},
        }),
    );
    assert.match(
        html,
        /snapshot name &#x27;before-upgrade&#x27; already exists/,
        `the backend's own error message must actually appear in the rendered output, got: ${html}`,
    );
});

// Structural regression guard for the wiring half of the fix (the part a
// component-level render test, by itself, can't observe): both dialogs must
// mount their confirm-step body across BOTH "confirm" and "error", the same
// shape as ReplaceDialog's `(step === "confirm" || step === "error")` gate
// above -- not just "confirm" (the shape that dropped the error paragraph).
// Same technique as the "seven dialogs gate on busy" test above: driving
// each dialog to a real backend-rejected state and re-rendering isn't
// possible with this project's tooling (no jsdom/act(), see the file header
// comment), so this reads the actual JSX gating both call sites use.
test("RecompressDialog and SnapshotDialog mount their confirm-step body for both \"confirm\" and \"error\" steps (same shape as ReplaceDialog's earlier fix)", () => {
    const source = fs.readFileSync(actionsDialogsPath, "utf8");
    assert.match(
        source,
        /\(step === "confirm" \|\| step === "error"\) && state && \(\s*<RecompressConfirmStep/,
        "RecompressDialog must mount RecompressConfirmStep on both confirm and error",
    );
    assert.match(
        source,
        /\(step === "confirm" \|\| step === "error"\) && state && \(\s*<SnapshotConfirmStep/,
        "SnapshotDialog must mount SnapshotConfirmStep on both confirm and error",
    );
});

// DestroyDialog follows the exact same ReplaceConfirmStep/RecompressConfirmStep
// split (earlier shape) -- `DestroyConfirmStep` is pulled out the same way so
// this test renders the real body the confirm/error screen uses and asserts
// the backend's own message text is actually present.
test("DestroyConfirmStep renders the backend's error message on state.step === \"error\" (not a bare shell)", async () => {
    const { DestroyConfirmStep } = await loadActionsDialogsModule();
    const errorState: TypedConfirmState<{ destroyed: string }> = {
        step: "error",
        confirmationText: "shr1",
        result: null,
        errorMessage: "group `shr1` has a mounted filesystem at `/mnt/shr_data` -- unmount failed: target is busy",
    };
    const html = renderToStaticMarkup(
        React.createElement(DestroyConfirmStep, {
            group: recompressGroupFixture(),
            destroyInput: { groupName: "shr1", zeroSuperblocks: false },
            confirmText: "shr1",
            onConfirmText: () => {},
            state: errorState,
            busy: false,
            canExecute: true,
            onCancel: () => {},
            onExecute: () => {},
        }),
    );
    assert.match(
        html,
        /unmount failed: target is busy/,
        `the backend's own error message must actually appear in the rendered output, got: ${html}`,
    );
});

test("DestroyDialog mounts DestroyConfirmStep for both \"confirm\" and \"error\" steps (earlier shape)", () => {
    const source = fs.readFileSync(actionsDialogsPath, "utf8");
    assert.match(
        source,
        /\(step === "confirm" \|\| step === "error"\) && state && pendingInput && \(\s*<DestroyConfirmStep/,
        "DestroyDialog must mount DestroyConfirmStep on both confirm and error",
    );
});

// The zero-superblocks checkbox is the one piece of destroy-specific state
// this dialog carries beyond the shared typed-confirm shape -- must default
// OFF (member superblocks stay recoverable) and must actually explain the
// tradeoff in Korean, not just toggle a silent flag.
test("DestroyDialog's review step renders the zero-superblocks checkbox unchecked by default, with the recoverability tradeoff spelled out", async () => {
    const { DestroyDialog } = await loadActionsDialogsModule();
    const html = renderToStaticMarkup(
        React.createElement(DestroyDialog, { group: recompressGroupFixture(), onClose: () => {}, onChanged: () => {} }),
    );
    const checkbox = html.match(/<input[^>]*type="checkbox"[^>]*>/)?.[0];
    assert.ok(checkbox, `expected a checkbox in the review step, got: ${html}`);
    assert.doesNotMatch(checkbox, /checked/, "zero-superblocks must default to unchecked (off)");
    assert.match(html, /could still be revived later/, "must state the recoverable-if-off tradeoff");
    assert.match(html, /it cannot be revived/, "must state the unrecoverable-if-on tradeoff");
});

// --- A dialog's own "done" panel never painted -----------------------
//
// `ExpandDialog`/`ReplaceDialog`/`RecompressDialog`/`SnapshotDialog`/
// `ScheduleDialog`/`ReconcileDialog`'s success panel, and `ScrubDialog`'s
// post-action status refresh, were gated behind a run handler shaped like:
// `setState(s); if (s.step === "done") onChanged();`. `onChanged` is
// app.tsx's `refresh`, which itself synchronously does `setState({kind:
// "loading"})` -- React 18 batches both updates into a single commit, so
// `Dashboard` -> `OperationsPanel` -> the open dialog unmount in the exact
// same paint that would have shown the done panel. Measured live in a
// browser (MutationObserver over reconcile/schedule-install): the done text matched
// in ZERO frames across the whole transition. All seven dialogs had this
// shape, not just the four (reconcile/schedule install/expand/disk replace)
// first suspected -- recompress and snapshot create have the identical
// pattern, and scrub has the analogous one (its post-action `load()` call is discarded
// the same way).
//
// Fix: each dialog now defers `onChanged()` from the run handler (which
// becomes `setChanged(true)`) to a `handleClose` wrapper invoked only when
// the operator actually dismisses the dialog -- by then there is nothing
// left to unmount out from under. Chosen over deleting the done panels
// because an operator confirming a privileged, real-system action (a
// multi-hour reshape, a disk replace, ...) needs an explicit, readable
// confirmation that it actually finished, not just an eventually-refreshed
// dashboard they have to go verify themselves.

type OperationsPanelComponent = React.ComponentType<{
    groups: GroupStatus[]; disks: DiskStatus[]; onChanged: () => void;
}>;

/**
 * Stands in for exactly the one app.tsx behavior this defect depends on:
 * `refresh()` (app.tsx ~223-241) synchronously sets `state.kind = "loading"`
 * before any async work, and `Dashboard` -- therefore `OperationsPanel` and
 * any dialog it has open -- only renders while `state.kind === "ready"`
 * (app.tsx ~295-302). Everything rendered under this harness is the real,
 * bundled `OperationsPanel` and its real dialog components; only the
 * app-level unmount gate that `onChanged` triggers is stood in for -- this
 * is not a local reimplementation of any dialog's own state machine.
 */
const makeAppShapedHarness = (OperationsPanel: OperationsPanelComponent) => (
    { groups, disks }: { groups: GroupStatus[]; disks: DiskStatus[] },
) => {
    const [loading, setLoading] = React.useState(false);
    if (loading)
        return null;
    return React.createElement(OperationsPanel, { groups, disks, onChanged: () => setLoading(true) });
};

/** `cockpit.ts`'s default export is captured once at process start (line 34
 * above) and never reassigned -- only `.spawn` is mutated in place -- so
 * every dialog spawned through a freshly bundled module still reads
 * whichever stub is current at call time. */
const setSpawnStub = (impl: (argv: string[], options: unknown) => Promise<string>) => {
    (globalThis as unknown as { window: { cockpit: { spawn: unknown } } }).window.cockpit.spawn = impl;
};

const findButtonByText = (renderer: ReactTestRenderer, text: string): ReactTestInstance => {
    const buttons = renderer.root.findAllByType("button");
    const match = buttons.find(b => b.props.children === text);
    if (!match) {
        const seen = buttons.map(b => JSON.stringify(b.props.children));
        throw new Error(`no <button> with exact text ${JSON.stringify(text)} found; saw: ${seen.join(", ")}`);
    }
    return match;
};

const containsText = (node: ReactTestRendererNode | ReactTestRendererJSON[] | null, text: string): boolean => {
    if (node === null)
        return false;
    if (typeof node === "string")
        return node.includes(text);
    if (Array.isArray(node))
        return node.some(n => containsText(n, text));
    return (node.children ?? []).some(c => containsText(c, text));
};

test("ReconcileDialog's done panel actually renders, and the deferred refresh only fires once the operator closes it", async () => {
    const { OperationsPanel } = await loadActionsDialogsModule();
    let spawnedArgv: string[] | null = null;
    setSpawnStub(async (argv: string[]) => {
        spawnedArgv = argv;
        return "reconcile: 0 groups had a pending resize";
    });

    const Harness = makeAppShapedHarness(OperationsPanel);
    let renderer!: ReactTestRenderer;
    await act(async () => {
        renderer = create(React.createElement(Harness, { groups: [], disks: [] }));
    });

    await act(async () => {
        findButtonByText(renderer, "Finish the expansion").props.onClick();
    });
    assert.ok(renderer.toJSON(), "ReconcileDialog should be open after clicking the trigger");

    await act(async () => {
        findButtonByText(renderer, "Run the expansion finish").props.onClick();
    });

    assert.deepEqual(spawnedArgv, ["shr-rs", "reconcile"], "the real reconcile command must actually have been spawned");
    const afterRun = renderer.toJSON();
    assert.ok(
        afterRun,
        "the dialog must still be mounted right after the action finishes -- app.tsx-shaped onChanged must not have fired yet",
    );
    assert.ok(
        containsText(afterRun, "The expansion finish is complete."),
        "the done panel's own text must actually be present in the render tree, not just reached-and-discarded",
    );

    await act(async () => {
        findButtonByText(renderer, "Close").props.onClick();
    });
    assert.equal(
        renderer.toJSON(),
        null,
        "closing the done panel must be what finally triggers the app.tsx-shaped refresh/unmount",
    );
});

test("ScheduleDialog's done panel actually renders, and the deferred refresh only fires once the operator closes it", async () => {
    const { OperationsPanel } = await loadActionsDialogsModule();
    let spawnedArgv: string[] | null = null;
    setSpawnStub(async (argv: string[]) => {
        spawnedArgv = argv;
        return "schedule install: installed 0 timers";
    });

    const Harness = makeAppShapedHarness(OperationsPanel);
    let renderer!: ReactTestRenderer;
    await act(async () => {
        renderer = create(React.createElement(Harness, { groups: [], disks: [] }));
    });

    await act(async () => {
        findButtonByText(renderer, "Install the schedule").props.onClick();
    });
    assert.ok(renderer.toJSON(), "ScheduleDialog should be open after clicking the trigger");

    await act(async () => {
        findButtonByText(renderer, "Run the install").props.onClick();
    });

    assert.deepEqual(spawnedArgv, ["shr-rs", "schedule", "install"], "the real schedule install command must actually have been spawned");
    const afterRun = renderer.toJSON();
    assert.ok(
        afterRun,
        "the dialog must still be mounted right after the action finishes -- app.tsx-shaped onChanged must not have fired yet",
    );
    assert.ok(
        containsText(afterRun, "The install is complete."),
        "the done panel's own text must actually be present in the render tree, not just reached-and-discarded",
    );

    await act(async () => {
        findButtonByText(renderer, "Close").props.onClick();
    });
    assert.equal(
        renderer.toJSON(),
        null,
        "closing the done panel must be what finally triggers the app.tsx-shaped refresh/unmount",
    );
});

// DestroyDialog is the newest of the eight and the one this feature adds --
// drives it the same interactive way as Reconcile/Schedule above, but also
// covers what those two don't have: a typed-name confirmation gate (a
// mismatched name must block execute() and spawn nothing -- the shape) and
// an optional flag (--zero-superblocks) that must reach the real spawn only
// when checked. Exercises `proceed()`/`execute()` through the real component
// tree, not the pure argv builder in isolation (the lesson).
test("DestroyDialog's done panel renders, refresh is deferred until close, a mismatched confirmation blocks execute, and --zero-superblocks reaches the real spawn only when checked", async () => {
    const { OperationsPanel } = await loadActionsDialogsModule();
    let spawnedArgv: string[] | null = null;
    setSpawnStub(async (argv: string[]) => {
        spawnedArgv = argv;
        return JSON.stringify({ destroyed: "shr1" });
    });

    const Harness = makeAppShapedHarness(OperationsPanel);
    let renderer!: ReactTestRenderer;
    await act(async () => {
        renderer = create(React.createElement(Harness, { groups: [recompressGroupFixture()], disks: [] }));
    });

    await act(async () => {
        findButtonByText(renderer, "Destroy").props.onClick();
    });
    assert.ok(renderer.toJSON(), "DestroyDialog should be open after clicking the trigger");

    // Check zero-superblocks before proceeding, to prove it survives through to the real spawn.
    await act(async () => {
        renderer.root.findByProps({ type: "checkbox" }).props.onChange({ target: { checked: true } });
    });

    await act(async () => {
        findButtonByText(renderer, "Next: confirm").props.onClick();
    });

    // A mismatched typed confirmation must keep execute disabled and spawn nothing (the shape).
    await act(async () => {
        renderer.root.findByProps({ placeholder: "shr1" }).props.onChange({ target: { value: "wrong-name" } });
    });
    assert.equal(
        findButtonByText(renderer, "Destroy the group (cannot be undone)").props.disabled,
        true,
        "a mismatched typed confirmation must keep the execute button disabled",
    );
    assert.equal(spawnedArgv, null, "nothing may spawn before a matching confirmation");

    // The matching group name unblocks it.
    await act(async () => {
        renderer.root.findByProps({ placeholder: "shr1" }).props.onChange({ target: { value: "shr1" } });
    });
    await act(async () => {
        findButtonByText(renderer, "Destroy the group (cannot be undone)").props.onClick();
    });

    assert.deepEqual(
        spawnedArgv,
        ["shr-rs", "destroy", "--name", "shr1", "--yes", "--json", "--zero-superblocks"],
        "must spawn the exact argv for the group the dialog was opened for, including --zero-superblocks since it was checked",
    );
    const afterRun = renderer.toJSON();
    assert.ok(
        afterRun,
        "the dialog must still be mounted right after the action finishes -- app.tsx-shaped onChanged must not have fired yet",
    );
    assert.ok(
        containsText(afterRun, "The group has been destroyed."),
        "the done panel's own text must actually be present in the render tree, not just reached-and-discarded",
    );

    await act(async () => {
        findButtonByText(renderer, "Close").props.onClick();
    });
    assert.equal(
        renderer.toJSON(),
        null,
        "closing the done panel must be what finally triggers the app.tsx-shaped refresh/unmount",
    );
});

// Structural guard for the wiring half of the fix, same technique as the
// earlier tests above: driving all seven dialogs' async flows interactively
// would substantially duplicate the two deep tests just above without
// covering anything they don't already prove about the mechanism. Reads the
// real source instead to confirm the same `handleClose` contract these two
// interactive tests proved for Reconcile/Schedule was applied uniformly to
// every dialog that has an `onChanged` -- not just the two the real-browser
// MutationObserver run happened to measure.
// Bumped to 8 when `DestroyDialog` was added -- it must follow the same
// earlier fix as every other dialog in this file.
test("every dialog with an onChanged defers it to a handleClose wrapper, not a raw onClose passed straight to Modal", () => {
    const source = fs.readFileSync(actionsDialogsPath, "utf8");

    const modalOnCloseHandleClose = source.match(/<Modal\b[^>]*\bonClose=\{handleClose\}/g) ?? [];
    assert.equal(
        modalOnCloseHandleClose.length,
        8,
        `expected all 8 dialogs' <Modal> to route through their own handleClose, found ${modalOnCloseHandleClose.length}: ${JSON.stringify(modalOnCloseHandleClose)}`,
    );

    const rawOnCloseToModal = source.match(/<Modal\b[^>]*\bonClose=\{onClose\}/g) ?? [];
    assert.equal(
        rawOnCloseToModal.length,
        0,
        `no dialog may pass its raw onClose prop straight to <Modal> anymore -- found ${rawOnCloseToModal.length}: ${JSON.stringify(rawOnCloseToModal)}`,
    );

    const handleCloseDefs = source.match(/const handleClose = \(\) => \{/g) ?? [];
    assert.equal(
        handleCloseDefs.length,
        8,
        `expected each of the 8 dialogs to define its own handleClose, found ${handleCloseDefs.length}`,
    );

    const deferredChanges = source.match(/setChanged\(true\)/g) ?? [];
    assert.equal(
        deferredChanges.length,
        8,
        `expected each of the 8 dialogs' success branch to defer via setChanged(true) instead of calling onChanged() directly, found ${deferredChanges.length}`,
    );
});

// The test above only covers <Modal onClose={...}> -- it says nothing about
// the 13 inner cancel/close buttons and ErrorPanel's onClose prop. Mutating any
// one of those from `onClick={handleClose}` back to `onClick={onClose}`
// (verified live: e.g. ExpandDialog's "The expansion is complete." done-panel close
// button) still passed the full suite before this test existed -- an
// operator who clicks that button after a real expand sees the success
// panel, but the dashboard silently never refreshes and keeps showing the
// stale pre-expand layout/disk data.
//
// The invariant (verified against the real source: every `onClose`
// occurrence below the shared Modal/ErrorPanel definitions is either the
// prop's own declaration in a dialog's signature, or the single `onClose();`
// call inside that dialog's own `handleClose` body -- never wired directly
// to a JSX callback): below Modal/ErrorPanel (the only components that
// legitimately wire their OWN onClose prop to their OWN button), none of the
// seven dialogs may wire its raw `onClose` prop straight to any JSX callback
// (`onClick={onClose}`, `<Modal onClose={onClose}>`,
// `<ErrorPanel onClose={onClose}>`, ...) -- every dismiss affordance must go
// through that dialog's own `handleClose` instead. One check across the
// whole dialogs region rather than one assertion per button, so it also
// catches this class of mistake in any future dialog added to this file.
test("no dialog wires its raw onClose prop directly to a JSX callback -- every dismiss affordance routes through handleClose", () => {
    const source = fs.readFileSync(actionsDialogsPath, "utf8");

    // Splits off the shared `Modal`/`ErrorPanel` definitions (lines ~71-112,
    // which legitimately do `onClick={onClose}` against their OWN prop) from
    // the seven dialog components below them.
    const scrubMarkerIndex = source.indexOf("// --- scrub");
    assert.ok(scrubMarkerIndex > 0, "expected to find the scrub dialog's section marker to split shared components from the dialogs below them");
    const dialogsSource = source.slice(scrubMarkerIndex);

    const rawWiring = dialogsSource.match(/=\{onClose\}/g) ?? [];
    assert.equal(
        rawWiring.length,
        0,
        `a dialog wired its raw onClose prop directly to a JSX callback instead of its own handleClose: ${JSON.stringify(rawWiring)}`,
    );
});
