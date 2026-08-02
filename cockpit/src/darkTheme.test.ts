/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * `resolveDarkMode` decides whether the plugin document carries
 * `.pf-v6-theme-dark`, and every PatternFly 6 token keys off that class. It
 * is pure (the DOM lives in `setDarkMode`/`initDarkTheme` around it), so it
 * is testable without jsdom -- which this project does not have.
 *
 * The matrix below deliberately pins BOTH sides of BOTH discriminators:
 *
 *   - explicit-vs-auto: "light" must stay light while the OS asks for dark,
 *     and "dark" must stay dark while the OS asks for light. Only checking
 *     the agreeing combinations would pass for a function that ignored
 *     `style` entirely and returned `prefersDark`.
 *   - auto-follows-OS: "auto" must return true AND false. Only checking one
 *     would pass for a function that ignored `prefersDark` and returned a
 *     constant.
 *
 * Either half alone is green against a broken implementation, which is the
 * failure this project keeps re-learning: if the light is green on both
 * sides of a discriminator, nothing was verified.
 */
import assert from "node:assert/strict";
import { test } from "node:test";

import { resolveDarkMode } from "./darkTheme.ts";

test("an explicit dark preference wins over an OS that asks for light", () => {
    assert.equal(resolveDarkMode("dark", false), true);
    assert.equal(resolveDarkMode("dark", true), true);
});

test("an explicit light preference wins over an OS that asks for dark", () => {
    // The discriminating case: a `return prefersDark` stub fails here.
    assert.equal(resolveDarkMode("light", true), false);
    assert.equal(resolveDarkMode("light", false), false);
});

test("auto defers to the OS preference in both directions", () => {
    assert.equal(resolveDarkMode("auto", true), true);
    assert.equal(resolveDarkMode("auto", false), false);
});

test("an unset shell:style behaves as auto, not as a hardcoded theme", () => {
    // Cockpit leaves the key absent until the user first picks a style, so
    // this is the state of a fresh session -- it must still track the OS.
    assert.equal(resolveDarkMode(null, true), true);
    assert.equal(resolveDarkMode(null, false), false);
    assert.equal(resolveDarkMode("", true), true);
    assert.equal(resolveDarkMode("", false), false);
});

test("an unrecognised shell:style falls back to the OS rather than to light", () => {
    // Forward compatibility with a Cockpit that grows a fourth style: the
    // safe default is the OS preference, matching upstream's own `else`.
    assert.equal(resolveDarkMode("sepia", true), true);
    assert.equal(resolveDarkMode("sepia", false), false);
});
