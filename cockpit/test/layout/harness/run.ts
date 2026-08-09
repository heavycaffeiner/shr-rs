/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * One audited run: set the viewport, install the harness, load the page, wait
 * until it has stopped moving, optionally open a dialog, measure, record, and
 * fail on what came back.
 *
 * The waiting is the part that decides whether a run means anything. A
 * measurement taken while a web font is still swapping, or while the dashboard
 * is still on its loading spinner, produces a full set of plausible numbers
 * about a page nobody will ever see.
 */

import { expect, test as base } from "@playwright/test";
import type { Page } from "@playwright/test";

import { type AuditResult, auditPage } from "../audit.ts";
import { exclusions } from "../exclusions.ts";
import { extremes } from "../fixtures/extremes.ts";
import { nominal } from "../fixtures/nominal.ts";
import type { Locale } from "./cockpitStub.ts";
import { writeRunReport } from "./report.ts";
import { type Theme, installHarness } from "./routes.ts";
import { type HarnessServer, startHarnessServer } from "./server.ts";

const FIXTURES = { nominal, extremes };

export type FixtureName = keyof typeof FIXTURES;

export interface RunSpec {
    /** Names what is on screen, so a failure reads as a place rather than an
     * index. */
    view: string;
    locale: Locale;
    theme: Theme;
    width: number;
    fixture: FixtureName;
    /** Runs after the dashboard has settled, to put a dialog on screen. */
    open?: (page: Page) => Promise<void>;
}

/** Worker-scoped so the matrix pays for one server per worker, not one per
 * run. */
export const test = base.extend<Record<never, never>, { harness: HarnessServer }>({
    // The empty pattern is Playwright's fixture signature, not an oversight:
    // this fixture depends on nothing and still has to declare the slot.
    // eslint-disable-next-line no-empty-pattern
    harness: [async ({}, use) => {
        const server = await startHarnessServer();
        await use(server);
        await server.close();
    }, { scope: "worker" }],
});

const runId = (spec: RunSpec): string =>
    [spec.view, spec.locale, spec.theme, `${spec.width}px`, spec.fixture].join("-");

export const auditRun = async (page: Page, pageUrl: string, spec: RunSpec): Promise<AuditResult> => {
    await page.setViewportSize({ width: spec.width, height: 900 });
    await installHarness(page, { locale: spec.locale, theme: spec.theme, fixture: FIXTURES[spec.fixture] });

    const consoleErrors: string[] = [];
    page.on("console", message => {
        if (message.type() === "error")
            consoleErrors.push(message.text());
    });
    page.on("pageerror", error => consoleErrors.push(error.message));

    await page.goto(pageUrl, { waitUntil: "load" });

    // The dashboard renders a spinner until both spawns have settled. Auditing
    // through that would measure a centred spinner and call the page clean.
    await expect(page.locator(".pf-v6-c-page__main-section").first()).toBeVisible();
    await expect(page.locator(".pf-v6-c-spinner")).toHaveCount(0, { timeout: 15_000 });

    if (spec.open)
        await spec.open(page);

    // A swapped-in web font changes every text box on the page, so no
    // measurement is valid until the swap has happened.
    await page.evaluate(() => document.fonts.ready.then(() => undefined));
    await page.waitForFunction(() => !document.querySelector(".pf-v6-c-spinner"));

    const result = await page.evaluate(auditPage, { locale: spec.locale, exclusions });

    const run = runId(spec);
    writeRunReport({
        run,
        view: spec.view,
        locale: spec.locale,
        theme: spec.theme,
        width: spec.width,
        fixture: spec.fixture,
        result,
    });

    // A React crash or a missing asset invalidates the measurement the same
    // way a missing stylesheet does, and neither shows up as a violation.
    expect(consoleErrors, `${run}: the page logged errors, so what was measured is not the page as shipped`).toEqual([]);

    const rendered = result.violations
            .map(violation => `  [${violation.check}] ${violation.element}\n      ${violation.detail}`)
            .join("\n");
    // Asserting the count, not the array: a `toEqual([])` diff prints every
    // violation as a serialized object and buries the message that says where
    // each one is.
    expect(result.violations.length, `${run}: layout violations\n${rendered}`).toBe(0);

    return result;
};

export { expect };
