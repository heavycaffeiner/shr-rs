// SPDX-License-Identifier: LGPL-2.1-or-later
//
// The create-group wizard's modal chrome was `Backdrop` > modal-box,
// missing the `Bullseye` that every other dialog in this plugin has
// (`actionsDialogs.tsx`'s shared `Modal` wrapper renders `Backdrop` >
// `Bullseye` > modal-box, and PatternFly's own `ModalBox` is used the same
// way). `Bullseye` is what centres the box; without it the box was laid out
// flush against the backdrop's start edge.
//
// The failure was easy to miss because nothing looked broken in isolation:
// modal-box's own `--pf-v6-c-modal-box--MaxWidth: calc(100% - spacer--xl)`
// still applied, so the box was correctly narrower than the frame -- it just
// put the whole 32px of intended gutter on one side. Measured in a real
// browser at a 1449px frame: box left 0, width 1417, i.e. 0px of gutter on
// the left and 32px on the right. A screenshot reads as "slightly off-centre
// dialog", not as a missing layout component, and no existing test rendered
// this file at all.
//
// Asserts the nesting ORDER, not just that both class names are present:
// the defect was purely structural, and a `Bullseye` rendered anywhere else
// in the tree (or as a sibling of the box rather than its parent) would
// centre nothing while still satisfying a presence-only check.
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import * as esbuild from "esbuild";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";

import type { DiskStatus } from "./model.ts";

// `cockpit.ts` throws at module load if `window.cockpit` is missing, and
// `createGroupWizard.tsx` imports it statically. The wizard only reaches
// `cockpit.spawn` from `startPreflight()`, which a static render never runs.
// Same stub, and the same reasoning, as actionsDialogs.test.ts.
(globalThis as unknown as { window: unknown }).window = { cockpit: { spawn: async () => "" } };

const srcDir = path.dirname(fileURLToPath(import.meta.url));

// Mirrors `loadActionsDialogsModule` in actionsDialogs.test.ts -- see the
// comments there for why each esbuild option is what it is (`loader: {".css":
// "empty"}` because PatternFly imports stylesheets as a side effect and
// `write: false` has nowhere to emit them; `mainFields` because PatternFly
// ships no `exports` map and the CJS tree cannot be satisfied with React
// external in ESM output; the temp file inside `src/` so bare specifiers
// resolve through this project's own node_modules).
const loadWizardModule = async (): Promise<{
    CreateGroupWizard: React.ComponentType<{
        disks: DiskStatus[];
        existingGroupNames: string[];
        existingGroupVgNames: { name: string; vg_name: string }[];
        onClose: () => void;
        onCreated: () => void;
    }>;
}> => {
    const result = await esbuild.build({
        entryPoints: [path.join(srcDir, "createGroupWizard.tsx")],
        bundle: true,
        format: "esm",
        platform: "node",
        write: false,
        external: ["react", "react-dom", "react-dom/*"],
        loader: { ".css": "empty" },
        mainFields: ["module", "main"],
    });
    const tmpFile = path.join(srcDir, `_createGroupWizard.esbuild-tmp.${process.pid}.mjs`);
    fs.writeFileSync(tmpFile, result.outputFiles[0].text);
    try {
        return await import(`file://${tmpFile.split("\\").join("/")}`) as never;
    } finally {
        fs.rmSync(tmpFile, { force: true });
    }
};

// Fully populated, with no `as DiskStatus` cast: the wizard maps over
// `disk.arrays`, so a cast-shortened fixture typechecks and then throws at
// render time on `undefined.length`.
const disk = (name: string, systemDisk: boolean): DiskStatus => ({
    name,
    size: 128_000_000_000,
    model: null,
    serial: null,
    rotational: true,
    smart: {
        state: "unknown",
        temperature_c: null,
        power_on_hours: null,
        pending_sectors: null,
        reallocated_sectors: null,
        uncorrectable_sectors: null,
        nvme_critical_warning: null,
    },
    arrays: [],
    system_disk: systemDisk,
});

const renderWizard = async (): Promise<string> => {
    const { CreateGroupWizard } = await loadWizardModule();
    return renderToStaticMarkup(
        React.createElement(CreateGroupWizard, {
            disks: [disk("/dev/vda", true), disk("/dev/vdb", false)],
            existingGroupNames: [],
            existingGroupVgNames: [],
            onClose: () => {},
            onCreated: () => {},
        }),
    );
};

test("the wizard centres its modal box in the backdrop, via Bullseye", async () => {
    const html = await renderWizard();

    // Guards the guard: if the wizard ever stops rendering a backdrop at all
    // (a switch to PatternFly's portal-based `Modal`, which renders as an
    // empty string under `renderToStaticMarkup` -- see this file's header and
    // createGroupWizard.tsx's), the ordering assertion below would pass
    // vacuously on two `-1`s.
    const backdrop = html.indexOf("pf-v6-c-backdrop");
    const bullseye = html.indexOf("pf-v6-l-bullseye");
    const modalBox = html.indexOf("pf-v6-c-modal-box");
    assert.ok(backdrop >= 0, "wizard rendered no pf-v6-c-backdrop");
    assert.ok(modalBox >= 0, "wizard rendered no pf-v6-c-modal-box");

    assert.ok(
        bullseye >= 0,
        "wizard rendered no pf-v6-l-bullseye: its modal box will sit flush against the backdrop's " +
        "start edge instead of centred, with the whole of modal-box's MaxWidth gutter on one side",
    );
    assert.ok(
        backdrop < bullseye && bullseye < modalBox,
        "expected the wizard's chrome to nest Backdrop > Bullseye > modal-box (the shape " +
        "actionsDialogs.tsx's Modal and PatternFly's own ModalBox both use), but the class names " +
        `appear in the order backdrop@${backdrop}, bullseye@${bullseye}, modal-box@${modalBox}`,
    );
});

// A modal-box with no size modifier is not "default sized" -- modal-box.css
// gives it `--Width: 100%` and `--MaxWidth: calc(100% - spacer--xl)`, so it fills
// the frame. Measured live before the fix: 1417px wide in a 1449px frame, for a
// dialog whose first step is a five-column disk table. Stock Cockpit never does
// this: `/users` and `/sosreport`, from two unrelated Cockpit packages, both
// render `pf-v6-c-modal-box pf-m-align-top pf-m-md` (840px, 305px gutters,
// measured in the same browser at the same frame width).
//
// Checks the box's own class attribute rather than searching the whole document
// for the modifier names: `pf-m-md` and `pf-m-align-top` are generic PatternFly
// modifiers that appear on many components, so a document-wide `includes` would
// keep passing if the modifiers landed on some nested card instead of the box.
test("the wizard's modal box carries Cockpit's own size and placement modifiers", async () => {
    const html = await renderWizard();

    const match = /<div class="(pf-v6-c-modal-box[^"]*)"/.exec(html);
    // Guards the guard: if the chrome is ever restructured so the box is no
    // longer a `div` with a leading `class` attribute, the assertions below
    // would have nothing to test and must fail loudly rather than silently.
    assert.ok(match, "found no <div class=\"pf-v6-c-modal-box...\"> in the wizard's rendered chrome");

    // Per-modifier consequence text: a single shared sentence about width
    // would misdescribe the failure whenever the placement modifier is the
    // one that went missing.
    const consequences: Record<string, string> = {
        "pf-m-md": "without a size modifier the box takes modal-box.css's --Width: 100% and goes full-bleed",
        "pf-m-align-top": "without it the box is vertically centred, so it jumps at every step transition",
    };
    const boxClass = match[1];
    for (const [modifier, consequence] of Object.entries(consequences)) {
        assert.ok(
            boxClass.split(" ").includes(modifier),
            `the wizard's modal box is missing ${modifier}; stock Cockpit's dialogs render ` +
            `"pf-v6-c-modal-box pf-m-align-top pf-m-md", but this one renders "${boxClass}" -- ${consequence}.`,
        );
    }
});
