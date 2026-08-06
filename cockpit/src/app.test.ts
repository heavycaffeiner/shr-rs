// SPDX-License-Identifier: LGPL-2.1-or-later
//
// app.tsx's own dashboard fetch (`status --json` + `fs df --json`) used
// `superuser: "try"` while every other spawn site in this project
// (`actions.ts`'s `SPAWN_OPTIONS`, `createGroup.ts`'s `SPAWN_OPTIONS`, both
// pinned by their own tests) uses `superuser: "require"`. Reproduced on a
// real guest: with `/var/lib/shr-rs/state.toml` owned `root:root 0600` (the
// normal post-install state), `"try"` silently ran `status --json`
// unprivileged instead of prompting for admin access, and the dashboard
// rendered nothing but "Permission denied (os error 13)" -- Cockpit's own
// "administrative access required" prompt never appeared. This was the one
// spawn call in the whole project no test covered.
//
// Same JSX-loader workaround as actionsDialogs.test.ts (see its header
// comment): Node's own type-stripping rejects the `.tsx` extension outright,
// so this bundles the real `app.tsx` with `esbuild` and dynamic-imports it.
// `window.cockpit` is stubbed only so `cockpit.ts`'s module-load-time check
// doesn't throw during that import -- the test never touches it, since
// `fetchDashboardState` below takes its `spawn` function as a parameter
// (same dependency-injection discipline as `actions.ts`/`createGroup.ts`),
// not off `window.cockpit` directly.
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import * as esbuild from "esbuild";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";

import type { Spawn } from "./actions.ts";
import type { SpawnOptions } from "./cockpit.ts";
import { installEnglishCatalog } from "./testCatalog.ts";

// The msgids in `src/` are dotted keys, so without a catalogue every string
// below would render as its key. See testCatalog.ts.
installEnglishCatalog();

(globalThis as unknown as { window: unknown }).window = { cockpit: { spawn: async () => "" } };

const srcDir = path.dirname(fileURLToPath(import.meta.url));
const appPath = path.join(srcDir, "app.tsx");

/** Bundles the real `app.tsx` (react/react-dom left external so this
 * process's own React instance is the one that runs, matching
 * actionsDialogs.test.ts's `loadActionsDialogsModule`) and loads it as a
 * throwaway ESM file. */
const loadAppModule = async (): Promise<{
    Application: React.ComponentType;
    fetchDashboardState: (spawn: Spawn) => Promise<{ output: string; fsDf: unknown }>;
    errorHintKind: (error: unknown) => "permission" | "installation";
}> => {
    const result = await esbuild.build({
        entryPoints: [appPath],
        bundle: true,
        format: "esm",
        platform: "node",
        write: false,
        external: ["react", "react-dom", "react-dom/*"],
        // See panels.test.ts: PatternFly components side-effect-import their
        // own CSS, which esbuild cannot place anywhere under `write: false`.
        loader: { ".css": "empty" },
        // Also per panels.test.ts: resolve PatternFly's ESM build, the one the
        // production bundle uses, not the CJS build `platform: "node"` prefers.
        mainFields: ["module", "main"],
    });
    const code = result.outputFiles[0].text;
    const tmpFile = path.join(srcDir, `_app.esbuild-tmp.${process.pid}.mjs`);
    fs.writeFileSync(tmpFile, code);
    try {
        return await import(`file://${tmpFile.split("\\").join("/")}`) as never;
    } finally {
        fs.rmSync(tmpFile, { force: true });
    }
};

/** Records every call so the test can assert on the exact options object
 * `refresh()` -> `fetchDashboardState()` actually hands to `cockpit.spawn`,
 * not on source text. */
class RecordingSpawn {
    calls: { argv: string[]; options: SpawnOptions }[] = [];
    fn: Spawn = async (argv, options) => {
        this.calls.push({ argv, options });
        if (argv[1] === "status")
            return JSON.stringify({ schema_version: 1, health: "ok", disks: [], arrays: [], groups: [] });
        // `fs df` is allowed to fail -- fetchDashboardState must swallow it
        // (see model.ts's CapacityOverviewPanel degrade-gracefully comment
        // this mirrors in app.tsx) and still resolve.
        throw new Error("fs df not stubbed for this test");
    };
}

test("app.tsx's dashboard fetch requires superuser for both status --json and fs df --json, never \"try\"", async () => {
    const { fetchDashboardState } = await loadAppModule();
    const recorder = new RecordingSpawn();
    await fetchDashboardState(recorder.fn);

    assert.equal(recorder.calls.length, 2, `expected exactly 2 spawn calls (status + fs df), got ${recorder.calls.length}: ${JSON.stringify(recorder.calls)}`);
    for (const call of recorder.calls) {
        assert.deepEqual(
            call.options,
            { err: "message", superuser: "require" },
            `spawn(${JSON.stringify(call.argv)}) used ${JSON.stringify(call.options)} -- the dashboard's own fetch must use { err: "message", superuser: "require" }, same as every other spawn site in this project`,
        );
    }
});

// The error panel's hint blamed a missing shr-rs install/PATH for
// EVERY dashboard-fetch failure, including a Cockpit-level privilege
// rejection. Reproduced in a real browser: logged into Cockpit as `dev` in
// limited-access mode (admin not activated), the `superuser: "require"`
// spawn rejects with `problem: "access-denied"`, `exit_status: null`
// (captured live from the plugin frame). Cockpit does not auto-prompt for
// privilege elevation here, so the error panel is the only thing the user
// sees, and the install/PATH hint sent them to diagnose a problem they
// don't have. The accompanying `message` is localized and was observed in
// two languages for one rejection, so it is deliberately NOT what
// `errorHintKind` keys off -- and deliberately not asserted below either.
// Both directions are pinned here so a hint that ALWAYS names one cause
// (the same defect shape in a new place) cannot pass.
test("errorHintKind names administrative access for an access-denied CockpitError", async () => {
    const { errorHintKind } = await loadAppModule();
    const rejection = { problem: "access-denied", message: "Not permitted to perform this action." };
    assert.equal(errorHintKind(rejection), "permission", `expected "permission" for ${JSON.stringify(rejection)}`);
});

test("errorHintKind still names installation/PATH for a not-found CockpitError", async () => {
    const { errorHintKind } = await loadAppModule();
    const rejection = { problem: "not-found", message: "shr-rs: not found" };
    assert.equal(errorHintKind(rejection), "installation", `expected "installation" for ${JSON.stringify(rejection)}`);
});

test("errorHintKind defaults to installation/PATH for an error with no problem code", async () => {
    const { errorHintKind } = await loadAppModule();
    assert.equal(errorHintKind(new Error("some other transport failure")), "installation");
});

// This plugin drew its own <Masthead>. PatternFly's `Page` puts its
// children inside `.pf-v6-c-page__main-container`, a grid item carrying
// `z-index: 100` -- and a grid item with a z-index forms a stacking context
// even at `position: static`. That trapped every descendant, including each
// dialog's fixed `Backdrop` (z 400) and `ModalBox` (z 500), at an effective
// 100, below the masthead's 300. Measured in a real browser on the
// create-group wizard: `elementsFromPoint` at the dialog title's centre
// returned the masthead's toolbar *above* `pf-v6-c-modal-box__title`, and the
// close button at (1356, 24) sat inside the masthead's 69px band, so it could
// not be clicked -- a lost control, not just a cosmetic overlap. Stock
// Cockpit pages render no masthead inside their frame (checked live against
// `/system` and `/system/services`, both of which hold only page sections);
// the shell draws the masthead above the iframe.
//
// Both halves are asserted because either alone would pass against a broken
// implementation: dropping the masthead is what unblocks the dialogs, and
// keeping all four header controls is what makes this a move rather than a
// deletion. `renderToStaticMarkup` runs no effects, so this is the initial
// `{ kind: "loading" }` state -- where the refresh button reads "Loading"
// and the health badge reads "Checking".
test("the dashboard draws no masthead of its own, and keeps every header control", async () => {
    const { Application } = await loadAppModule();
    const html = renderToStaticMarkup(React.createElement(Application));

    assert.doesNotMatch(
        html,
        /pf-v6-c-masthead/,
        "a Cockpit plugin must not draw its own masthead: the shell already draws one above the iframe, and ours (z-index 300) rendered over every dialog header trapped at z 100 by pf-v6-c-page__main-container",
    );

    // The four controls the masthead used to carry. These must survive the
    // move into the leading page section.
    assert.match(html, /SHR-RS RAID Manager/, "the page title must survive the masthead removal");
    assert.match(html, /Pools disks of different sizes/, "the subtitle must survive the masthead removal");
    assert.match(html, /Create group/, "the create-group button must survive the masthead removal");
    assert.match(html, /Loading/, "the refresh button must survive the masthead removal (loading state label)");
    assert.match(html, /Checking/, "the health badge must survive the masthead removal (loading state label)");

    // Dropping the masthead silently took the page's full-width grid area
    // with it. page.css hands the main container that area via
    // `.pf-v6-c-page.pf-m-no-sidebar, .pf-v6-c-masthead + .pf-v6-c-page__main-container, ...`,
    // so the masthead had been supplying it through the sibling arm of that
    // selector. With neither, `.pf-v6-c-page` keeps its `"sidebar main"`
    // columns and reserves 290px for a sidebar this plugin never renders --
    // measured live as a 290px indent on every page. Cockpit sets the same
    // modifier on its own pages, so this pins the replacement rather than a
    // workaround.
    assert.match(
        html,
        /class="[^"]*\bpf-v6-c-page\b[^"]*\bpf-m-no-sidebar\b/,
        "with no masthead, the page must carry pf-m-no-sidebar or PatternFly's grid reserves 290px for a sidebar that is never rendered",
    );
});

// The capacity allocation card rendered 197px tall around 227px of content on
// a phone, so it grew its own scrollbar and cut the caveat text off mid-line.
// Reported from a real phone, then reproduced at 390px in a real Cockpit
// session on the guest.
//
// Every horizontal-overflow check this project had was blind to it. Those look
// for elements sticking out past the viewport; this content was clipped INSIDE
// a scroll container, so nothing stuck out and `scrollWidth` never grew.
//
// The cause is nested column flexboxes. `.pf-v6-c-page__main` is
// `display: flex; flex-direction: column` with a viewport-height `height` and
// `overflow-y: auto`, so each `PageSection` is a flex ITEM that can be shrunk.
// A PatternFly `Card` sets `overflow-y: auto`, which makes it a scroll
// container, and a scroll container's `min-height: auto` resolves to 0 instead
// of to its content height -- so the card is the one box in the section that
// CAN absorb an over-constraint, and it does so silently.
//
// Measured in the browser, not guessed: `flex-shrink: 0` on the section does
// not help, and neither does PatternFly's own `pf-m-no-fill` (`isFilled={false}`
// sets `flex-grow` only), nor `flex-shrink` on the inner Flex or its children,
// nor `flex-wrap`, `align-content` or `min-height` on the column. Taking the
// section out of flex formatting is the only thing that worked.
//
// This asserts the class survives, which is all a DOM-free test can do here.
// The layout itself has no assertion available without a real browser, which is
// exactly why the defect reached a phone.
test("the content page section is block-formatted so cards cannot be flex-shrunk", async () => {
    const { Application } = await loadAppModule();
    const html = renderToStaticMarkup(React.createElement(Application));

    const sections = html.match(/class="[^"]*pf-v6-c-page__main-section[^"]*"/g) ?? [];
    assert.ok(
        sections.length >= 2,
        `expected the header and content page sections, found ${sections.length}: ${JSON.stringify(sections)}`,
    );
    assert.ok(
        sections.some(cls => cls.includes("pf-v6-u-display-block")),
        "the section holding the dashboard cards must be block-formatted (pf-v6-u-display-block); as a column-flex item it shrinks its cards, and a PatternFly Card is a scroll container, so the loss shows up as a scrollbar inside the card instead of as overflow. Sections found: " +
        JSON.stringify(sections),
    );
});
