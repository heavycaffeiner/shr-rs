/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * gettext bindings for the plugin.
 *
 * The msgids passed to `_()` below are stable dotted keys
 * ("panels.drives.col.node"), not English sentences, so English is a
 * translation like any other: `po/en.po` and `po/ko.po` each supply the
 * whole UI. See cockpit/README.md for why, and build.js for how the English
 * catalogue reaches a session Cockpit considers untranslated.
 *
 * There is nothing to initialise here. `index.html` loads `po-default.js` and
 * `po.js` between `../base1/cockpit.js` and our bundle; both call
 * `cockpit.locale()` before a single line of ours runs, so `cockpit.gettext`
 * already answers in the session language by first render.
 *
 * This reads the `cockpit` global directly instead of importing `cockpit.ts`,
 * which is the one place in `src/` that does. `cockpit.ts` throws at module
 * load when the API is absent -- correct for a module whose only purpose is
 * `spawn`, and wrong for this one: `model.ts` and `actions.ts` are pure
 * modules that `node --test` imports directly, with no `window` at all (see
 * model.test.ts), and they still have strings to translate. Reading the
 * global lazily is what keeps translation from dragging a browser requirement
 * into those tests; those load `po/en.po` themselves (src/testCatalog.ts), so
 * the fallbacks below are the "no catalogue at all" path and return the key.
 */

import type { CockpitApi } from "./cockpit.ts";

/* `window.cockpit` on a page; plain `undefined` under `node --test`. */
const api = (): CockpitApi | undefined => (globalThis as { cockpit?: CockpitApi }).cockpit;

/** Puts the session language on `<html>`, which index.html can only guess at
 * because it is a static file. Screen readers and the browser's own font
 * fallback both key off it, and it is the one piece of the page that
 * `cockpit.gettext` cannot fix on its own. */
export const applyDocumentLanguage = (): void => {
    document.documentElement.lang = api()?.language ?? "en";
    document.documentElement.dir = api()?.language_direction ?? "ltr";
};

/** Looks a message key up in the session catalogue. One of the `xgettext`
 * keywords in the Makefile -- which is what puts the key in the `.pot` -- so
 * the name is this short. */
export const _ = (key: string): string => api()?.gettext?.(key) ?? key;

/** A counted noun needs its own key pair, because English splits singular
 * from plural where Korean does not: `po/en.po` fills both `msgstr[0]` and
 * `msgstr[1]`, `po/ko.po` declares `nplurals=1` and fills one. */
export const ngettext = (key1: string, keyN: string, n: number): string =>
    api()?.ngettext?.(key1, keyN, n) ?? (n === 1 ? key1 : keyN);

/** Interpolation has to happen *after* the catalogue lookup -- a translator
 * needs to move `$0` to where their language puts it -- so values go through
 * here rather than through a template literal. The fallback handles the
 * positional `$0` form this package uses; `cockpit.format` itself also takes
 * named keys, which nothing here does. */
export const format = (format_string: string, ...args: unknown[]): string =>
    api()?.format?.(format_string, ...args) ??
    format_string.replace(/\$(\d+)/g, (whole, index: string) => {
        const value = args[Number(index)];
        return value === undefined ? whole : String(value);
    });
