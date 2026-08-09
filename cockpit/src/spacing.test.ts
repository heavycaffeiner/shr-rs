/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Holds the plugin's spacing on a 4px grid at the one point where authorship is
 * knowable. A value found in this scan was written by this project; a value
 * measured in a browser was composed from ours and PatternFly's and cannot be
 * attributed to either. That split is why this file and `test/layout/` both
 * exist rather than one replacing the other.
 *
 * The grid is not a house preference. PatternFly 6.6.0's spacer scale is
 * `0.25rem 0.5rem 1rem 1.5rem 2rem 3rem 4rem 5rem`, which at a 16px root is
 * `4 8 16 24 32 48 64 80`, and all sixteen semantic spacer tokens alias into
 * it. Measured on the built `dist/index.css`, 2163 of its 2626 spacing
 * declarations are `var()` references and the rest are `0`, `auto` or
 * `initial`; the only six em/px literals are in Font Awesome legacy rules and a
 * tree-view table toggle, none of which this plugin renders. So the framework
 * is already on the grid, and the only way off it is for this package's own
 * source to name a value.
 *
 * The scan finds nothing today. That is the point: it is a ratchet against a
 * future edit, not a cleanup. The cost of not having one is on record, in that
 * a `pf-v6-u-mb-2` typo lands with no error at all -- a class that does not
 * exist in the stylesheet is not a CSS error, it is simply ignored, so the
 * author sees no spacing and no complaint. `test/layout/`'s P3 catches the
 * missing class; A3 below catches the malformed step name before a build.
 */
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const srcDir = path.dirname(fileURLToPath(import.meta.url));

/** The resolved PatternFly spacer scale in px at a 16px root, which is what
 * makes 4 the grid unit rather than an arbitrary choice. */
export const GRID_UNIT = 4;

/** Scale steps a `pf-v6-u-{m,p}*` class may name. `auto` is handled separately
 * because it is only meaningful on a margin. */
const SCALE_STEPS = ["none", "xs", "sm", "md", "lg", "xl", "2xl", "3xl", "4xl"];

/** The breakpoints PatternFly's `-on-*` suffix and its breakpoint prop objects
 * both accept. There is no `-on-default`; the unsuffixed form is the default. */
const BREAKPOINTS = ["sm", "md", "lg", "xl", "2xl"];

/**
 * Strips comments, tracking block state across lines.
 *
 * `tokens.test.ts` does this per line with a `^\s*(//|/*|*)` test, which is
 * right for the JSDoc blocks it had to stop firing on. It is not enough here.
 * `app.tsx` and `panels.tsx` carry long `{/* ... *\/}` JSX comments recording
 * browser measurements, and their continuation lines start with prose rather
 * than with `*`. Those lines contain most of the `px` literals in this package
 * ("reproduced at 390px", "197px tall around 227px of content"), so a per-line
 * heuristic would report a dozen violations that are all measurements someone
 * wrote down on purpose.
 *
 * Returns one entry per input line so line numbers survive, with comment spans
 * blanked rather than removed.
 *
 * Known limit, and it is a false negative rather than a false positive: `//`
 * inside a string literal (a URL) truncates the rest of that line. Nothing is
 * wrongly reported; at worst something is missed, on a line that already
 * contains a URL.
 */
export const stripComments = (source: string): string[] => {
    const lines: string[] = [];
    let inBlock = false;

    for (const line of source.split("\n")) {
        let kept = "";
        let at = 0;

        while (at < line.length) {
            if (inBlock) {
                const close = line.indexOf("*/", at);
                if (close === -1)
                    break;
                inBlock = false;
                at = close + 2;
                continue;
            }

            const lineComment = line.indexOf("//", at);
            const blockOpen = line.indexOf("/*", at);

            if (lineComment !== -1 && (blockOpen === -1 || lineComment < blockOpen)) {
                kept += line.slice(at, lineComment);
                break;
            }
            if (blockOpen !== -1) {
                kept += line.slice(at, blockOpen);
                inBlock = true;
                at = blockOpen + 2;
                continue;
            }
            kept += line.slice(at);
            break;
        }

        lines.push(kept);
    }

    // Guards the stripper itself. An unterminated `/*` would silently blank
    // every remaining line of the file and turn this whole scan green, which is
    // the one failure mode a comment-aware scanner adds over a per-line one.
    assert.ok(!inBlock, "unterminated block comment: the rest of the file was skipped, so this scan proved nothing");

    return lines;
};

export interface Offence {
    rule: "A1" | "A2" | "A3" | "A4";
    line: number;
    text: string;
    why: string;
}

/** `v` sits on the 4px grid. Exact here, unlike the runtime checks, because a
 * source literal is a number someone typed rather than a layout result. */
const onGrid = (px: number): boolean => Math.abs(px) % GRID_UNIT === 0;

/*
 * A1. No inline styles, in any form.
 *
 * This is what makes A2 and A3 sufficient rather than merely indicative: with
 * `style=` banned outright there is no remaining route by which the source can
 * set a length that the class and literal scans cannot see. It costs nothing
 * today, because `src/` contains none.
 */
const findInlineStyles = (line: string): string | null => {
    const match = /\bstyle\s*=\s*\{/.exec(line);
    return match ? match[0] : null;
};

/*
 * A2. Length literals.
 *
 * `px` must be a multiple of 4. `rem` must be a multiple of 0.25, which is the
 * same grid expressed in the unit the tokens are actually written in. `em` is
 * rejected outright: it resolves against the inherited font size, so there is
 * no fixed number to compare against a grid at all.
 *
 * The only two live matches in this package are `minWidths={{ default: "220px" }}`
 * in `app.tsx` and `panels.tsx`. 220 is 4 x 55, so both pass.
 */
const LENGTH = /(?<![\w])(\d+(?:\.\d+)?|\.\d+)(px|rem|em)\b/gi;

const findLengths = (line: string): Offence["why"][] => {
    const problems: string[] = [];
    for (const match of line.matchAll(LENGTH)) {
        const value = Number(match[1]);
        const unit = match[2].toLowerCase();

        if (unit === "em")
            problems.push(`${match[0]} is relative to the inherited font size, so it has no fixed size to place on the grid`);
        else if (unit === "px" && !onGrid(value))
            problems.push(`${match[0]} is not a multiple of ${GRID_UNIT}px`);
        else if (unit === "rem" && (value * 4) % 1 !== 0)
            problems.push(`${match[0]} is not a multiple of 0.25rem, which is the ${GRID_UNIT}px grid unit`);
    }
    return problems;
};

/*
 * A3. Spacing utility classes.
 *
 * PatternFly ships the families `m mt mr mb ml mx my p pt pr pb pl px py`, each
 * with the nine scale steps, `auto`, and an optional `-on-{breakpoint}` suffix.
 *
 * `auto` is accepted on a margin and rejected on a padding. PatternFly
 * generates `pf-v6-u-p-auto-on-md` for symmetry, but `padding: auto` is not a
 * value: it is invalid and computes to 0. A padding class naming it is always a
 * mistake, and one that renders as "no padding" rather than as an error.
 *
 * The `(t|r|b|l|x|y)?-` shape is what keeps this off `pf-v6-u-min-width`, which
 * `createGroupWizard.tsx` uses to release a fieldset's `min-content` floor: `m`
 * is followed by `i`, which is neither a side letter nor the required hyphen.
 */
const SPACING_CLASS = /pf-v6-u-(m|p)(t|r|b|l|x|y)?-([a-z0-9]+)(?:-on-([a-z0-9]+))?/g;

const findSpacingClasses = (line: string): string[] => {
    const problems: string[] = [];
    for (const match of line.matchAll(SPACING_CLASS)) {
        const [whole, family, , step, breakpoint] = match;

        if (step === "auto") {
            if (family === "p")
                problems.push(`${whole} names \`auto\`, which is not a valid padding value and computes to 0`);
        } else if (!SCALE_STEPS.includes(step)) {
            problems.push(`${whole} names the step \`${step}\`, which is not on the spacer scale (${SCALE_STEPS.join(", ")})`);
        }

        if (breakpoint !== undefined && !BREAKPOINTS.includes(breakpoint))
            problems.push(`${whole} names the breakpoint \`${breakpoint}\`, which is not one of ${BREAKPOINTS.join(", ")}`);
    }
    return problems;
};

/*
 * A4. Spacer props.
 *
 * PatternFly's layout components take spacing as a breakpoint object of named
 * constants, never as a raw length. `gap`, `rowGap` and `columnGap` accept the
 * bare family name (their default gap); `spacer` and `spaceItems` require a
 * step. Read off `Flex.d.ts` in the pinned 6.6.0 tree.
 *
 * Typing already rejects an unknown constant, so this rule is not duplicating
 * `tsc`. What it adds is the case typing cannot see: a value assembled at
 * runtime, or one widened to `string` on its way in.
 */
const SPACER_PROP = /\b(spacer|spaceItems|gap|rowGap|columnGap)\s*=\s*\{\{([^}]*)\}\}/g;
const GAPPY = ["gap", "rowGap", "columnGap"];

const findSpacerProps = (line: string): string[] => {
    const problems: string[] = [];
    for (const match of line.matchAll(SPACER_PROP)) {
        const [whole, prop, body] = match;
        const suffixes = SCALE_STEPS.map(step => step[0].toUpperCase() + step.slice(1));
        const allowed = new RegExp(`^${prop}(${suffixes.join("|")})${GAPPY.includes(prop) ? "?" : ""}$`);

        for (const [, key] of body.matchAll(/(?:^|,)\s*['"]?([\w'"]+?)['"]?\s*:/g)) {
            const clean = key.replace(/['"]/g, "");
            if (clean !== "default" && !BREAKPOINTS.includes(clean))
                problems.push(`${whole.slice(0, 60)} keys on \`${clean}\`, which is not \`default\` or one of ${BREAKPOINTS.join(", ")}`);
        }

        const values = [...body.matchAll(/:\s*["']([^"']*)["']/g)].map(hit => hit[1]);
        if (values.length === 0)
            problems.push(`${whole.slice(0, 60)} passes no string constant, so the value is assembled rather than named`);

        for (const value of values) {
            if (!allowed.test(value))
                problems.push(`${prop} is given \`${value}\`, which is not one of PatternFly's ${prop} constants`);
        }
    }
    return problems;
};

/** Exported so the assertions below can show this scanner rejecting things. A
 * scanner nobody has watched reject anything is indistinguishable from one
 * whose patterns never match. */
export const findSpacingOffences = (source: string): Offence[] => {
    const offences: Offence[] = [];
    const push = (rule: Offence["rule"], line: number, text: string, why: string): void => {
        offences.push({ rule, line, text: text.trim(), why });
    };

    stripComments(source).forEach((line, index) => {
        const lineNo = index + 1;

        const inlineStyle = findInlineStyles(line);
        if (inlineStyle !== null)
            push("A1", lineNo, line, "an inline style can set any length, and nothing else in this scan can see it");

        for (const why of findLengths(line))
            push("A2", lineNo, line, why);
        for (const why of findSpacingClasses(line))
            push("A3", lineNo, line, why);
        for (const why of findSpacerProps(line))
            push("A4", lineNo, line, why);
    });

    return offences;
};

test("no source file sets a spacing value off the 4px grid", () => {
    const sources = fs.readdirSync(srcDir)
            .filter(name => (name.endsWith(".ts") || name.endsWith(".tsx")) && !name.endsWith(".test.ts"));

    // Guards the guard, same as `patternfly.test.ts` and `tokens.test.ts`: an
    // empty file list would make the assertion below vacuously true and this
    // test would go on passing while checking nothing at all.
    assert.ok(
        sources.length >= 8,
        `expected to scan the plugin sources under ${srcDir}, found ${sources.length}: ${JSON.stringify(sources)}`,
    );

    const offenders = sources.flatMap(name => (
        findSpacingOffences(fs.readFileSync(path.join(srcDir, name), "utf8"))
                .map(hit => `${name}:${hit.line} [${hit.rule}] ${hit.why}`)
    ));

    assert.deepEqual(
        offenders,
        [],
        "A spacing value is set off the PatternFly spacer scale. Every gap, padding and margin in this " +
        "package comes from a component prop or a `pf-v6-u-*` utility, and those resolve to the scale " +
        "4/8/16/24/32/48/64/80px. A value outside it lands between grid lines against everything around " +
        "it, with no build error and nothing failing. Offenders:\n" + offenders.join("\n"),
    );
});

test("the scanner rejects the forms it claims to reject", () => {
    const rules = (source: string): string[] => findSpacingOffences(source).map(hit => hit.rule);

    // A1
    assert.deepEqual(rules('<div style={{ padding: 6 }}>'), ["A1"]);
    assert.deepEqual(rules('<div style={styles.card}>'), ["A1"]);

    // A2
    assert.deepEqual(rules('minWidths={{ default: "6px" }}'), ["A2"]);
    assert.deepEqual(rules('const W = "0.3rem";'), ["A2"]);
    assert.deepEqual(rules('const W = "1.5em";'), ["A2"]);

    // A3
    assert.deepEqual(rules('className="pf-v6-u-mb-2"'), ["A3"]);
    assert.deepEqual(rules('className="pf-v6-u-p-auto"'), ["A3"]);
    assert.deepEqual(rules('className="pf-v6-u-mt-md-on-phone"'), ["A3"]);

    // A4
    assert.deepEqual(rules('<Flex spaceItems={{ default: "spaceItemsTiny" }}>'), ["A4"]);
    assert.deepEqual(rules('<Flex gap={{ phone: "gapMd" }}>'), ["A4"]);
    assert.deepEqual(rules('<Flex spaceItems={{ default: spacing }}>'), ["A4"]);
});

test("the scanner does not fire on the spacing this package really contains", () => {
    // The two live length literals. 220 is 4 x 55.
    assert.deepEqual(findSpacingOffences('<Gallery hasGutter minWidths={{ default: "220px" }}>'), []);
    // The five live spacing utility classes.
    assert.deepEqual(findSpacingOffences('className="pf-v6-u-mb-md"'), []);
    assert.deepEqual(findSpacingOffences('<FlexItem className="pf-v6-u-mt-xs">'), []);
    // `min-width` is not a margin. This is the near-miss the family regex has
    // to survive, and `createGroupWizard.tsx` really uses it.
    assert.deepEqual(findSpacingOffences('className="pf-v6-u-min-width"'), []);
    // Non-spacing utilities in the same namespace.
    assert.deepEqual(findSpacingOffences('const MONO = "pf-v6-u-font-family-monospace";'), []);
    assert.deepEqual(findSpacingOffences('const ACTION_ROW = "pf-v6-u-flex-wrap";'), []);
    assert.deepEqual(findSpacingOffences('className="pf-v6-u-text-color-subtle"'), []);
    assert.deepEqual(findSpacingOffences('className="pf-v6-u-display-block"'), []);
    // The four live spacer props, all breakpoint objects of named constants.
    assert.deepEqual(findSpacingOffences('<Flex spaceItems={{ default: "spaceItemsXs" }}>'), []);
    assert.deepEqual(findSpacingOffences('<Flex spaceItems={{ default: "spaceItemsLg" }}>'), []);
    assert.deepEqual(findSpacingOffences('<Flex gap={{ default: "gap" }}>'), []);
    assert.deepEqual(findSpacingOffences('<Flex rowGap={{ md: "rowGap2xl" }}>'), []);
    // `hasGutter` takes no value, so it is not this rule's business.
    assert.deepEqual(findSpacingOffences('<Split hasGutter isWrappable>'), []);
});

test("comment spans are stripped, including the JSX blocks this package writes measurements in", () => {
    // Every one of these is real prose from `app.tsx`, `panels.tsx` or
    // `actionsDialogs.tsx`. A per-line comment test would report all of them.
    const jsxBlock = [
        "            {/* `pf-v6-u-display-block` is load-bearing, and the defect",
        "                Reported from a phone and then reproduced at 390px: the",
        "                card rendered 197px tall around 227px of content, so it",
        "                grew its own scrollbar. */}",
        '            <PageSection className="pf-v6-u-display-block">',
    ].join("\n");
    assert.deepEqual(findSpacingOffences(jsxBlock), []);

    assert.deepEqual(findSpacingOffences("// `pf-m-md`: 840px wide with 305px gutters in a 1449px frame"), []);
    assert.deepEqual(findSpacingOffences("/* a 111px term column left the value column 135px */"), []);
    assert.deepEqual(
        findSpacingOffences("/*\n * this row demanded 340px inside a 322px body\n */"),
        [],
        "a JSDoc continuation line is only a comment because the block above it opened one",
    );

    // ...but the same value in real code still fails, so stripping comments
    // narrowed the rule rather than gutting it.
    assert.deepEqual(findSpacingOffences('const W = "390px";').map(hit => hit.rule), ["A2"]);
    assert.deepEqual(
        findSpacingOffences('/* 390px */ const W = "390px";').map(hit => hit.rule),
        ["A2"],
        "a comment on the same line must not shield the code beside it",
    );

    // Line numbers survive stripping, which is the whole reason blanked lines
    // are kept rather than dropped.
    assert.deepEqual(findSpacingOffences('/*\n *\n */\nconst W = "6px";')[0].line, 4);
});

test("the stripper refuses to pass an unterminated block comment", () => {
    // Without this the stripper would blank the rest of the file and every rule
    // above would report a clean scan of nothing.
    assert.throws(() => stripComments("/* opened and never closed\nconst W = \"6px\";"), /unterminated block comment/);
});
