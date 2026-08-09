/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * The three requests index.html makes that no static server can answer, plus
 * the theme choice.
 *
 * `po.js` is the one worth reading twice. cockpit-ws does not serve a file of
 * that name: it negotiates on the session language and, for any language it
 * considers untranslated -- English included, because this package's msgids
 * are dotted keys -- it answers 200 with an EMPTY body. Serving `po.en.js`
 * here instead would apply English twice and audit a load order no session
 * ever sees, so the English case really does fulfil with nothing.
 */

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import type { Page } from "@playwright/test";

import { type HarnessFixture, type Locale, stubSource } from "./cockpitStub.ts";

const DIST = path.resolve(fileURLToPath(new URL("../../../dist", import.meta.url)));

export type Theme = "light" | "dark";

export interface HarnessOptions {
    locale: Locale;
    theme: Theme;
    fixture: HarnessFixture;
}

export const installHarness = async (page: Page, options: HarnessOptions): Promise<void> => {
    // `darkTheme.ts` reads `localStorage["shell:style"]` at module scope, so
    // this has to be in place before a single line of the bundle runs.
    // `prefers-color-scheme` is not the lever: PatternFly 6 switches on the
    // `.pf-v6-theme-dark` class, and only an explicit style sets it here.
    await page.addInitScript(`localStorage.setItem("shell:style", ${JSON.stringify(options.theme)});`);

    await page.route("**/base1/cockpit.js", route => route.fulfill({
        status: 200,
        contentType: "text/javascript; charset=utf-8",
        body: stubSource(options.fixture),
    }));

    // Cockpit's own branding, which lives outside any package. Empty rather
    // than absent: a 404 would leave a console error in every run and prove
    // nothing, and its real content is host-specific styling this package
    // must not depend on.
    await page.route("**/static/branding.css", route => route.fulfill({
        status: 200,
        contentType: "text/css; charset=utf-8",
        body: "",
    }));

    await page.route("**/shr-rs/po.js", route => route.fulfill({
        status: 200,
        contentType: "text/javascript; charset=utf-8",
        body: options.locale === "ko" ? fs.readFileSync(path.join(DIST, "po.ko.js"), "utf8") : "",
    }));
};
