/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Button names, per locale, read from the same `.po` files the bundle ships.
 *
 * The dialog specs have to click "Scrub" in an English run and "스크럽" in a
 * Korean one. Hard-coding either string would make the Korean matrix silently
 * skip half its dialogs the first time a translation is reworded, so the names
 * come from the catalogue that produced the button.
 */

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import { po } from "gettext-parser";

import type { Locale } from "./cockpitStub.ts";

const PO_DIR = path.resolve(fileURLToPath(new URL("../../../po", import.meta.url)));

const cache = new Map<Locale, Map<string, string>>();

const load = (locale: Locale): Map<string, string> => {
    const parsed = po.parse(fs.readFileSync(path.join(PO_DIR, `${locale}.po`)), { defaultCharset: "utf-8" });
    const catalog = new Map<string, string>();
    for (const context of Object.values(parsed.translations)) {
        for (const [msgid, translation] of Object.entries(context)) {
            const text = translation.msgstr[0];
            if (msgid && text)
                catalog.set(msgid, text);
        }
    }
    return catalog;
};

export const translate = (locale: Locale, msgid: string): string => {
    let catalog = cache.get(locale);
    if (!catalog) {
        catalog = load(locale);
        cache.set(locale, catalog);
    }
    const text = catalog.get(msgid);
    if (!text)
        throw new Error(`po/${locale}.po has no translation for "${msgid}", so the layout audit cannot find the control it names`);
    return text;
};
