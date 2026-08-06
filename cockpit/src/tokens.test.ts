/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Mechanises the rule that `ui.tsx`'s header and `index.tsx`'s stylesheet
 * comment both state in prose: this package has no design system of its own,
 * and every colour comes from a PatternFly token by way of a component or a
 * `pf-v6-u-*` utility class.
 *
 * That rule is not stylistic. The plugin used to ship `app.scss`, a hand-copied
 * PatternFly 4/5 palette, and the copies drifted from the shell they sat beside
 * (light background `#f4f4f4` against the shell's `#f2f2f2`, dark foreground
 * `#f0f0f0` against the shell's `#fff`). Nothing failed; the page rendered, and
 * the mismatch was only visible with both documents on screen at once. A rule
 * that can only be enforced by eye is one this project has already broken, so
 * it is enforced here instead.
 *
 * Dark mode is why it matters now specifically. A literal colour is a value the
 * `.pf-v6-theme-dark` class cannot reach: it renders identically in both
 * themes, which for a foreground colour means it eventually renders illegibly
 * in one of them. There is no build error and no runtime error for that, only a
 * screenshot nobody took.
 */
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const srcDir = path.dirname(fileURLToPath(import.meta.url));

/*
 * Each pattern is anchored tightly enough not to fire on the hex-adjacent text
 * `src/` legitimately contains. The one live example is `panels.tsx`'s NVMe
 * critical-warning bitmask, rendered via `.toString(16)`, which is a call
 * expression and never a `#`-prefixed literal, so nothing below matches it. The negative
 * assertions at the bottom of this file pin that.
 *
 * The `(?![0-9a-f])` tail on each hex form is what keeps `#abc` from also
 * matching inside `#abcdef`, so a 6-digit literal is reported once rather than
 * twice.
 */
const COLOUR_PATTERNS: { name: string; pattern: RegExp }[] = [
    { name: "#rgb / #rgba", pattern: /#[0-9a-f]{3,4}(?![0-9a-f])/i },
    { name: "#rrggbb", pattern: /#[0-9a-f]{6}(?![0-9a-f])/i },
    { name: "#rrggbbaa", pattern: /#[0-9a-f]{8}(?![0-9a-f])/i },
    {
        name: "colour function",
        pattern: /\b(?:rgba?|hsla?|hwb|lab|lch|oklab|oklch|color-mix)\s*\(/i,
    },
];

/*
 * Comment lines are skipped, and that is not a loophole; it is the rule
 * stated precisely. What this test forbids is DECLARING a colour, and a
 * comment declares nothing: it cannot reach the rendered page in either theme,
 * which is the entire basis of the objection. `ui.tsx`'s own header is the
 * proof that the distinction is needed rather than hypothetical. It documents
 * the historical drift by quoting the exact values involved ("light background
 * `#f4f4f4` against the shell's `#f2f2f2`"), and that paragraph is the clearest
 * statement of the rule anywhere in this package. A scanner that made the file
 * explaining the rule the file that violates it would be deleted within the
 * day, which is the same as not having one.
 *
 * The test deliberately does NOT strip trailing comments from a code line. That
 * would need real tokenisation to avoid mangling a string containing `//` (a
 * URL, a path), and the miss it admits is a colour literal in a trailing
 * comment, which is still a comment, still unrendered, still harmless.
 */
const isCommentLine = (line: string): boolean => /^\s*(?:\/\/|\/\*|\*)/.test(line);

/** Exported for the assertions below, which are the only way to show this test
 * can actually fail: a scanner nobody has seen reject anything is
 * indistinguishable from one that matches nothing at all. */
export const findColourLiterals = (source: string): { line: number; text: string; kind: string }[] => {
    const hits: { line: number; text: string; kind: string }[] = [];
    source.split("\n").forEach((line, index) => {
        if (isCommentLine(line))
            return;
        for (const { name, pattern } of COLOUR_PATTERNS) {
            const match = pattern.exec(line);
            if (match)
                hits.push({ line: index + 1, text: match[0], kind: name });
        }
    });
    return hits;
};

test("no source file names a colour directly", () => {
    const sources = fs.readdirSync(srcDir)
            .filter(name => (name.endsWith(".ts") || name.endsWith(".tsx")) && !name.endsWith(".test.ts"));

    // Guards the guard, same as `patternfly.test.ts`: if the scan ever finds
    // nothing (a moved directory, a renamed extension), an empty file list
    // would make the assertion below vacuously true and this test would go on
    // passing while checking nothing at all.
    assert.ok(
        sources.length >= 8,
        `expected to scan the plugin sources under ${srcDir}, found ${sources.length}: ${JSON.stringify(sources)}`,
    );

    const offenders = sources.flatMap(name => (
        findColourLiterals(fs.readFileSync(path.join(srcDir, name), "utf8"))
                .map(hit => `${name}:${hit.line} ${hit.kind} -> ${hit.text.trim()}`)
    ));

    assert.deepEqual(
        offenders,
        [],
        "A colour is named directly instead of coming from a PatternFly token. A literal colour " +
        "cannot respond to the `.pf-v6-theme-dark` class, so it renders identically in light and " +
        "dark and eventually renders illegibly in one of them, with no build or test error, which " +
        "is exactly how the old app.scss palette drifted from the Cockpit shell. Use a PatternFly " +
        "component or a `pf-v6-u-*` utility class instead. Offenders:\n" + offenders.join("\n"),
    );
});

test("the scanner rejects the literal forms it claims to reject", () => {
    // Without this, a scanner whose regexes never matched anything would report
    // a clean tree forever and read as green.
    assert.equal(findColourLiterals('color: "#fff"').length, 1);
    assert.equal(findColourLiterals('color: "#f4f4f4"').length, 1);
    assert.equal(findColourLiterals('color: "#ff00ff80"').length, 1);
    assert.equal(findColourLiterals("background: rgb(244, 244, 244)").length, 1);
    assert.equal(findColourLiterals("background: rgba(0,0,0,.5)").length, 1);
    assert.equal(findColourLiterals("border-color: hsl(210 8% 95%)").length, 1);
    assert.equal(findColourLiterals("fill: oklch(0.7 0.1 200)").length, 1);
});

test("the scanner does not fire on the hex-adjacent code this package really contains", () => {
    // `panels.tsx` renders an NVMe critical-warning bitmask this way. A scanner
    // that flagged it would be reverted within the day, which is the same as
    // not having one.
    assert.deepEqual(findColourLiterals("disk.smart.nvme_critical_warning.toString(16)"), []);
    // Utility and token names are how a colour is SUPPOSED to be referenced
    // here, so none of them may register as a violation.
    assert.deepEqual(findColourLiterals('className="pf-v6-u-text-color-subtle"'), []);
    assert.deepEqual(findColourLiterals('const MONO = "pf-v6-u-font-family-monospace"'), []);
    assert.deepEqual(findColourLiterals('<Label isCompact color="orange">'), []);
    // A prose mention of a colour in a comment is not a declaration of one.
    assert.deepEqual(findColourLiterals("// the colour key the legend used to draw by hand"), []);
    // The three comment shapes `ui.tsx`'s own header actually uses. If any of
    // these registered, the file that documents this rule would be the file
    // that fails it.
    assert.deepEqual(findColourLiterals(" * light background `#f4f4f4` against the shell's `#f2f2f2`"), []);
    assert.deepEqual(findColourLiterals("/* dark foreground #f0f0f0 against the shell's #fff */"), []);
    assert.deepEqual(findColourLiterals("    // drifted to #eee"), []);
    // ...but the same value in a real declaration still fails, so skipping
    // comments narrowed the rule rather than gutting it.
    assert.equal(findColourLiterals('    const BG = "#f4f4f4";').length, 1);
});
