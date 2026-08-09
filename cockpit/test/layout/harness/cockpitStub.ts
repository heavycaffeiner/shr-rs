/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Stands in for `../base1/cockpit.js`, which only a real Cockpit session
 * serves. Reproduces the four pieces this package actually consumes:
 * `locale()` merging, `gettext`/`ngettext`/`format`, the `language` and
 * `language_direction` properties, and `spawn`.
 *
 * The gettext half is not decoration. The audit's whole Korean case rests on
 * the page really rendering Korean, and `po.ko.js` is a `cockpit.locale()`
 * call -- so getting the merge order and the plural indexing wrong would
 * produce a page that looks translated in places and audits nothing. P4 in
 * `audit.ts` re-checks the outcome from the rendered DOM rather than trusting
 * this file.
 */

import type { FsDfReport, StatusReport } from "../../../src/model.ts";

export type Locale = "en" | "ko";

export interface HarnessFixture {
    /** Answers `shr-rs status --json`. */
    status: StatusReport;
    /** Answers `shr-rs fs df --json`; `null` makes that call reject, which
     * `fetchDashboardState` swallows into a degraded capacity panel. */
    fsDf: FsDfReport | null;
}

/** The JavaScript `../base1/cockpit.js` is answered with. Everything the page
 * needs is inlined, so the harness holds no second file to keep in step. */
export const stubSource = (fixture: HarnessFixture): string => `
(function () {
    "use strict";

    var catalog = {};
    var pluralForms = function (n) { return n === 1 ? 0 : 1; };

    function locale(po) {
        if (!po) {
            catalog = {};
            return;
        }
        Object.assign(catalog, po);
        var header = po[""];
        if (!header)
            return;
        if (typeof header["plural-forms"] === "function")
            pluralForms = header["plural-forms"];
        if (header.language)
            api.language = header.language;
        if (header["language-direction"])
            api.language_direction = header["language-direction"];
    }

    /* Cockpit's own entry shape: [context, msgstr0, msgstr1, ...]. Index 0 is
       the context marker, never a translation, which is why the lookups below
       start at 1. */
    function gettext(context, message) {
        if (message === undefined) {
            message = context;
            context = undefined;
        }
        var entry = catalog[context ? context + "\\u0004" + message : message];
        return entry && entry[1] ? entry[1] : message;
    }

    function ngettext(message1, messageN, n) {
        var entry = catalog[message1];
        if (entry) {
            var translated = entry[pluralForms(n) + 1];
            if (translated !== undefined && translated !== null)
                return translated;
        }
        return n === 1 ? message1 : messageN;
    }

    function format(template, ...args) {
        var lookup = (args.length === 1 && args[0] !== null && typeof args[0] === "object")
            ? args[0]
            : args;
        return String(template).replace(/\\$\\{([^}]+)\\}|\\$([-\\w]+)/g, function (whole, braced, bare) {
            var value = lookup[braced !== undefined ? braced : bare];
            return value === undefined || value === null ? "" : String(value);
        });
    }

    var FIXTURE = ${JSON.stringify(fixture)};

    function spawn(argv) {
        var key = argv.join(" ");
        if (key === "shr-rs status --json")
            return Promise.resolve(JSON.stringify(FIXTURE.status));
        if (key === "shr-rs fs df --json") {
            return FIXTURE.fsDf === null
                ? Promise.reject({ problem: null, message: "fs df is unavailable in this fixture", exit_status: 1, exit_signal: null })
                : Promise.resolve(JSON.stringify(FIXTURE.fsDf));
        }
        /* Shaped like a real Cockpit rejection so the caller's own error
           classification runs. "not-found" is Cockpit's code for an argv it
           could not execute, which is exactly what an unstubbed command is. */
        return Promise.reject({
            problem: "not-found",
            message: "the layout harness does not stub: " + key,
            exit_status: null,
            exit_signal: null,
        });
    }

    var api = {
        locale: locale,
        gettext: gettext,
        ngettext: ngettext,
        format: format,
        spawn: spawn,
        language: "en",
        language_direction: "ltr",
    };

    window.cockpit = api;
})();
`;
