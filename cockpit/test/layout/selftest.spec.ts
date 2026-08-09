/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Drives every check to failure on markup built to break it.
 *
 * A layout audit that only ever runs against a page it passes is
 * indistinguishable from one whose checks silently return nothing, and the
 * difference only shows up on the day a real defect lands. Each case here is
 * the smallest document that trips one check, and asserts the audit reports
 * that check and no other.
 */

import { expect, test } from "@playwright/test";

import { type CheckId, auditPage } from "./audit.ts";

/** Wrapped so the preconditions pass: P4 reads `lang`, and everything below it
 * measures a page that is already known to be the right one. */
const page4 = (body: string, style = ""): string =>
    `<!DOCTYPE html><html lang="en"><head><meta charset="utf-8">
     <style>* { margin: 0; padding: 0; font: 16px/1.3 sans-serif } ${style}</style>
     </head><body>${body}</body></html>`;

interface Case {
    check: CheckId;
    what: string;
    html: string;
    /** Substring the failure has to name, so a check that fires for the wrong
     * reason is not mistaken for a pass. */
    detail: string;
}

const CASES: Case[] = [
    {
        check: "P3",
        what: "a PatternFly class no rule names",
        html: page4(`<div class="pf-v6-c-invented">x</div>`),
        detail: "pf-v6-c-invented",
    },
    {
        check: "B1",
        what: "a spacer utility realized off the scale",
        html: page4(`<div class="pf-v6-u-mt-wrong">x</div>`, ".pf-v6-u-mt-wrong { margin-top: 7px }"),
        detail: "7px",
    },
    {
        check: "B2",
        what: "a stacked sibling that does not share its leading edge",
        html: page4(`<div id="stack"><p>one</p><p id="off">two</p><p>three</p></div>`,
                    "#stack { display: block; width: 300px } #off { position: relative; left: 7px }"),
        detail: "starts at x=7",
    },
    {
        check: "B3",
        what: "a flex child sitting off the row's align-items",
        html: page4(`<div id="row"><span>a</span><span id="off">b</span><span>c</span></div>`,
                    "#row { display: flex; align-items: center; height: 60px } " +
                    "#row span { height: 20px } #off { position: relative; top: 5px }"),
        detail: "align-items: center",
    },
    {
        check: "B5",
        what: "a baseline row whose first lines do not sit on one line",
        html: page4(`<div id="row"><span>aaa</span><span id="off">bbb</span><span>ccc</span></div>`,
                    "#row { display: flex; align-items: baseline } #off { position: relative; top: 3px }"),
        detail: "align-items: baseline",
    },
    {
        check: "B6",
        what: "content clipped by its own container",
        html: page4(`<div id="clip">Supercalifragilisticexpialidocious</div>`,
                    "#clip { width: 40px; overflow: hidden; white-space: nowrap }"),
        detail: "clipped",
    },
    {
        check: "B7a",
        what: "a box that paints its edge and insets its content under 4px",
        html: page4(`<div id="card"><p>x</p></div>`,
                    "#card { background: #ddd; padding: 2px; width: 200px }"),
        detail: "2px",
    },
    {
        check: "B7b",
        what: "stacked siblings a sub-4px gap apart",
        html: page4(`<div id="stack"><p>one</p><p id="near">two</p></div>`,
                    "#stack { width: 300px } #near { margin-top: 2px }"),
        detail: "2px",
    },
    {
        check: "B7c",
        what: "a control under the 24x24 hit target minimum with a neighbour inside its spacing",
        // Two of them, flush, because one alone would take WCAG's spacing
        // exception. Flush rather than a small gap so B7b stays quiet, and
        // stripped of border and background because a default button paints
        // its own edge and would trip B7a on the user agent's padding.
        html: page4(`<div id="pair"><button aria-label="a">x</button><button aria-label="b">y</button></div>`,
                    "#pair { display: flex } " +
                    "#pair button { width: 20px; height: 20px; font-size: 10px; border: 0; background: none }"),
        detail: "20x20",
    },
];

for (const bad of CASES) {
    test(`${bad.check} fires on ${bad.what}`, async ({ page }) => {
        await page.setContent(bad.html, { waitUntil: "load" });
        await page.evaluate(() => document.fonts.ready.then(() => undefined));

        const result = await page.evaluate(auditPage, { locale: "en" as const, exclusions: [] });
        const rendered = result.violations
                .map(violation => `  [${violation.check}] ${violation.element}\n      ${violation.detail}`)
                .join("\n");

        const hit = result.violations.filter(violation => violation.check === bad.check);
        expect(hit.length, `expected ${bad.check}, got:\n${rendered}`).toBeGreaterThan(0);
        expect(hit.map(violation => violation.detail).join("\n")).toContain(bad.detail);

        // The audit has to name the defect it was given and nothing else, or a
        // green matrix run means only that the page dodged whatever noise the
        // checks emit.
        const others = [...new Set(result.violations.map(v => v.check))].filter(check => check !== bad.check);
        expect(others, `stray checks fired:\n${rendered}`).toEqual([]);
    });
}

test("P4 fires when an English run renders Hangul", async ({ page }) => {
    await page.setContent(page4("<p>디스크</p>"), { waitUntil: "load" });
    const result = await page.evaluate(auditPage, { locale: "en" as const, exclusions: [] });
    expect(result.violations.filter(v => v.check === "P4").map(v => v.detail)
            .join("\n"))
            .toContain("rendered Hangul");
});

test("P4 fires when a Korean run renders none", async ({ page }) => {
    await page.setContent(page4("<p>Disks</p>").replace('lang="en"', 'lang="ko"'), { waitUntil: "load" });
    const result = await page.evaluate(auditPage, { locale: "ko" as const, exclusions: [] });
    expect(result.violations.filter(v => v.check === "P4").map(v => v.detail)
            .join("\n"))
            .toContain("no Hangul at all");
});

test("P4 fires when the document declares the wrong language", async ({ page }) => {
    await page.setContent(page4("<p>Disks</p>"), { waitUntil: "load" });
    const result = await page.evaluate(auditPage, { locale: "ko" as const, exclusions: [] });
    expect(result.violations.filter(v => v.check === "P4").map(v => v.detail)
            .join("\n"))
            .toContain('<html lang> is "en"');
});

/* P5 asks whether the machine running the browser has a CJK font, and no
 * markup can take one away: a page that asks for a font nobody has still gets
 * the system fallback. So what is asserted here is the discriminator P5 is
 * built on, measured on this machine: a codepoint no font carries has to
 * measure differently from Hangul, or P5 could never tell the two apart. The
 * condition itself is reachable on a runner with no CJK font installed. */
test("P5's tofu probe separates a real glyph from a missing one", async ({ page }) => {
    await page.setContent(page4("<p>Disks</p>"), { waitUntil: "load" });
    await page.evaluate(() => document.fonts.ready.then(() => undefined));

    const widths = await page.evaluate(() => {
        const measure = (text: string): number => {
            const probe = document.createElement("span");
            probe.style.cssText = "position:absolute;visibility:hidden;white-space:pre;font-size:16px";
            probe.textContent = text;
            document.body.append(probe);
            const width = probe.getBoundingClientRect().width;
            probe.remove();
            return width;
        };
        return { hangul: measure("가".repeat(10)), tofu: measure("\uE000".repeat(10)) };
    });

    expect(Math.abs(widths.hangul - widths.tofu),
           `가 measured ${widths.hangul}px against ${widths.tofu}px for the missing-glyph box`).toBeGreaterThan(1);

    // And with a CJK font present, a Korean run is clean on P5.
    const result = await page.evaluate(auditPage, { locale: "ko" as const, exclusions: [] });
    expect(result.violations.filter(v => v.check === "P5")).toEqual([]);
});
