// SPDX-License-Identifier: LGPL-2.1-or-later
//
// `BandRow` (panels.tsx) rendered a band's 동기화/스크럽 cell as "유휴"
// (idle) whenever `band.sync` was `null` -- but `band.sync === null` covers
// TWO different situations (see `GroupBandStatus.sync`'s doc comment in
// crates/shr-command/src/report.rs): (a) a live mdadm array that genuinely
// has nothing syncing right now, and (b) no live mdadm array with this
// `md_name` AT ALL (e.g. state.toml survived a reboot but the array never
// reassembled). Confirmed on a real guest: stopping `/dev/md0` while
// state.toml stayed intact still rendered "유휴" in this cell, directly
// above an mdadm inventory panel correctly saying "구성된 mdadm 어레이가
// 없습니다." `band.members.length === 0` is the same "no live array" signal
// the member cell right next to it already reads (line 361's "실시간 멤버
// 정보 없음") and that `crates/shr-command/src/render.rs`'s
// `render_band_detail_row`/`watch_band_row` already guard on for the exact
// same field -- this brings `panels.tsx` in line with that precedent.
//
// Same esbuild-bundle-then-`react-dom/server` technique as
// actionsDialogs.test.ts (see that file's header for why: no jsdom in this
// project, and `node --test` can't load `.tsx` directly).
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

import * as esbuild from "esbuild";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";

import type { ArrayStatus, GroupStatus, StatusReport } from "./model.ts";

const srcDir = path.dirname(fileURLToPath(import.meta.url));
const panelsPath = path.join(srcDir, "panels.tsx");

/** Bundles the real `panels.tsx` (react/react-dom left external so this
 * process's own React instance is the one that runs) and loads it as a
 * throwaway ESM file. */
const loadPanelsModule = async (): Promise<{
    GroupsPanel: React.ComponentType<{ report: StatusReport }>;
}> => {
    const result = await esbuild.build({
        entryPoints: [panelsPath],
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
        // platform, i.e. `["module", "main"]`), so the test and the shipped
        // plugin exercise the same PatternFly code.
        mainFields: ["module", "main"],
    });
    const code = result.outputFiles[0].text;
    const tmpFile = path.join(srcDir, `_panels.esbuild-tmp.${process.pid}.mjs`);
    fs.writeFileSync(tmpFile, code);
    try {
        return await import(`file://${tmpFile.split("\\").join("/")}`) as never;
    } finally {
        fs.rmSync(tmpFile, { force: true });
    }
};

const bandFixture = (overrides: Partial<GroupStatus["bands"][number]> = {}): GroupStatus["bands"][number] => ({
    index: 0,
    level: "raid5",
    md_name: "md0",
    usable_bytes: 8_000_000_000_000,
    resize_pending: false,
    members: [],
    sync: null,
    last_scrub: null,
    scrub_in_progress: false,
    ...overrides,
});

const groupFixture = (overrides: Partial<GroupStatus> = {}): GroupStatus => ({
    name: "shr1",
    mode: "shr",
    layout_version: 1,
    mount_point: "/mnt/shr_data",
    fs_uuid: "abc-123",
    usable_bytes: 8_000_000_000_000,
    resize_pending: false,
    disks: [],
    bands: [bandFixture()],
    ...overrides,
});

const reportFixture = (overrides: Partial<StatusReport> = {}): StatusReport => ({
    schema_version: 2,
    health: "healthy",
    disks: [],
    arrays: [],
    groups: [groupFixture()],
    state_path: null,
    ...overrides,
});

test("BandRow does not render a dead band (no live members, no live sync) as idle -- it must say no live array", async () => {
    const { GroupsPanel } = await loadPanelsModule();
    const report = reportFixture({
        groups: [groupFixture({ bands: [bandFixture({ members: [], sync: null })] })],
        arrays: [], // no live array named md0 either -- matches the real observation
    });
    const html = renderToStaticMarkup(React.createElement(GroupsPanel, { report }));

    // The real defect: a dead band's sync/scrub cell reading the same as a
    // genuinely idle live one. This must not happen.
    assert.doesNotMatch(
        html,
        />유휴</,
        `a band with no live members must not render its sync cell as plain "유휴" (idle), got: ${html}`,
    );
    assert.match(
        html,
        /실시간 어레이 정보 없음/,
        `a band with no live members must say no live array exists, got: ${html}`,
    );
});

test("BandRow still renders a genuinely idle LIVE band (has live members, no sync in progress) as 유휴", async () => {
    const { GroupsPanel } = await loadPanelsModule();
    const arrays: ArrayStatus[] = [{
        name: "md0",
        level: "raid5",
        state: "clean",
        read_only: false,
        degraded: false,
        raid_disks: 3,
        active_disks: 3,
        members: ["sdb1", "sdc1", "sdd1"],
        sync: null,
    }];
    const report = reportFixture({
        groups: [groupFixture({ bands: [bandFixture({ members: ["sdb1", "sdc1", "sdd1"], sync: null })] })],
        arrays,
    });
    const html = renderToStaticMarkup(React.createElement(GroupsPanel, { report }));

    assert.match(html, />유휴</, `a live band that's genuinely idle must still say so, got: ${html}`);
    assert.doesNotMatch(
        html,
        /실시간 어레이 정보 없음/,
        `a live idle band must not also claim no live array exists, got: ${html}`,
    );
});
