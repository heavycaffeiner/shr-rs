/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Test-only: installs `po/en.po` as the session catalogue.
 *
 * The msgids in `src/` are dotted keys, so with no catalogue loaded `_()`
 * returns the key itself and every rendered string collapses to
 * "model.duration.hoursMinutes". That would quietly hollow out the tests that
 * exist to pin real defects -- "a 9-hour ETA must read 9 h 0 min, never
 * 540.0 min" cannot be asserted against a key. Loading the English catalogue
 * gives those assertions their subject back, and makes a key that is missing
 * from `po/en.po` fail a test rather than reach a user.
 *
 * Not imported by anything under `src/` that ships: the bundle is built from
 * `index.tsx`, and nothing on that graph reaches this file.
 */

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { po } from "gettext-parser";

type Catalog = Map<string, string[]>;

const load = (): Catalog => {
    const poPath = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "po", "en.po");
    const parsed = po.parse(fs.readFileSync(poPath), { defaultCharset: "utf-8" });
    const catalog: Catalog = new Map();
    for (const context of Object.values(parsed.translations)) {
        for (const [msgid, translation] of Object.entries(context)) {
            if (msgid)
                catalog.set(msgid, translation.msgstr);
        }
    }
    return catalog;
};

/** Mirrors `cockpit.format`'s positional `$0` substitution -- the only form
 * this package uses. */
const format = (formatString: string, ...args: unknown[]): string =>
    formatString.replace(/\$(\d+)/g, (whole, index: string) => {
        const value = args[Number(index)];
        return value === undefined ? whole : String(value);
    });

/**
 * Defines the `cockpit` global `i18n.ts` reads. Independent of the
 * `window.cockpit` stub the component tests set up for `spawn`: `i18n.ts`
 * deliberately does not go through `cockpit.ts`, so translation works in the
 * pure-module tests that have no `window` at all.
 */
export const installEnglishCatalog = (): void => {
    const catalog = load();
    (globalThis as { cockpit?: unknown }).cockpit = {
        gettext: (message: string) => catalog.get(message)?.[0] || message,
        // en.po declares `nplurals=2; plural=(n != 1)`.
        ngettext: (message1: string, messageN: string, n: number) =>
            catalog.get(message1)?.[n === 1 ? 0 : 1] || (n === 1 ? message1 : messageN),
        format,
        language: "en",
        language_direction: "ltr",
    };
};
