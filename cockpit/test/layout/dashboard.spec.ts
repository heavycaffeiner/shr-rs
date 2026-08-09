/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * The dashboard, across both locales, both themes, both viewports and both
 * fixtures: 16 runs.
 *
 * The fixture axis is not padding. `nominal` proves the layout works; only
 * `extremes` puts a 47 character by-id name next to a Korean label and finds
 * out whether the column holds.
 */

import { auditRun, test } from "./harness/run.ts";
import type { FixtureName } from "./harness/run.ts";
import type { Locale } from "./harness/cockpitStub.ts";
import type { Theme } from "./harness/routes.ts";

const LOCALES: Locale[] = ["en", "ko"];
const THEMES: Theme[] = ["light", "dark"];
const WIDTHS = [1280, 390];
const FIXTURES: FixtureName[] = ["nominal", "extremes"];

for (const locale of LOCALES) {
    for (const theme of THEMES) {
        for (const width of WIDTHS) {
            for (const fixture of FIXTURES) {
                test(`dashboard ${locale} ${theme} ${width}px ${fixture}`, async ({ page, harness }) => {
                    await auditRun(page, harness.pageUrl, { view: "dashboard", locale, theme, width, fixture });
                });
            }
        }
    }
}
