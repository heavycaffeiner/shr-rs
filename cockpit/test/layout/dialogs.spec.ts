/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * The nine dialogs, in both locales and both viewports: 36 runs.
 *
 * Theme and fixture are held fixed here. A dialog's box is sized by the modal
 * classes and its content is mostly this package's own prose, so the axis that
 * actually moves it is the one that changes the length of that prose. The
 * dashboard matrix covers the other two.
 */

import { expect } from "@playwright/test";
import type { Page } from "@playwright/test";

import { translate } from "./harness/catalog.ts";
import type { Locale } from "./harness/cockpitStub.ts";
import { auditRun, test } from "./harness/run.ts";

/** msgid of the control that opens each dialog. */
const DIALOGS: Record<string, string> = {
    "create-group": "app.action.createGroup",
    scrub: "dialogs.panel.scrub",
    expand: "dialogs.panel.expand",
    replace: "dialogs.panel.replace",
    snapshot: "dialogs.panel.snapshot",
    recompress: "dialogs.panel.recompress",
    destroy: "dialogs.panel.destroy",
    reconcile: "dialogs.reconcile.title",
    schedule: "dialogs.panel.schedule",
};

const LOCALES: Locale[] = ["en", "ko"];
const WIDTHS = [1280, 390];

const openDialog = (locale: Locale, msgid: string) => async (page: Page): Promise<void> => {
    // Exact, and not `.first()`: two controls answering to one name is itself
    // a finding, and swallowing it here would audit whichever one Playwright
    // happened to reach.
    await page.getByRole("button", { name: translate(locale, msgid), exact: true }).click();
    await expect(page.locator("[role=dialog]")).toBeVisible();
};

for (const [view, msgid] of Object.entries(DIALOGS)) {
    for (const locale of LOCALES) {
        for (const width of WIDTHS) {
            test(`dialog ${view} ${locale} ${width}px`, async ({ page, harness }) => {
                await auditRun(page, harness.pageUrl, {
                    view: `dialog-${view}`,
                    locale,
                    theme: "light",
                    width,
                    fixture: "nominal",
                    open: openDialog(locale, msgid),
                });
            });
        }
    }
}
