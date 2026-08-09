/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * The layout audit runs on its own runner, apart from `npm test`. The two
 * measure different things: `node:test` reads the source, this reads a
 * rendered page, and only the second needs a browser and a built `dist/`.
 */

import { defineConfig, devices } from "@playwright/test";

export default defineConfig({
    testDir: "./test/layout",
    globalSetup: "./test/layout/harness/globalSetup.ts",
    globalTeardown: "./test/layout/harness/globalTeardown.ts",
    fullyParallel: true,
    // A green run has to mean the page is clean, and `test.only` left in a
    // commit would make it mean "the one case someone was debugging".
    forbidOnly: Boolean(process.env.CI),
    // Retrying a layout measurement hides a flake instead of reporting it.
    // Every number here is deterministic given the same bundle and browser.
    retries: 0,
    reporter: process.env.CI ? [["list"], ["github"]] : [["list"]],
    use: {
        ...devices["Desktop Chrome"],
        // P2 asserts this from inside the page. Setting it here is what makes
        // that assertion pass rather than a coincidence of the host display.
        deviceScaleFactor: 1,
        // Nothing leaves the machine: every request is either the local
        // harness server or a `page.route` fulfilment.
        offline: false,
        trace: "retain-on-failure",
    },
    projects: [
        { name: "chromium", use: { ...devices["Desktop Chrome"], deviceScaleFactor: 1 } },
    ],
});
