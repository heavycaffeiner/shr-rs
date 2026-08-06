// SPDX-License-Identifier: LGPL-2.1-or-later
//
// The PatternFly packages were installed at two different releases, and
// the mismatch was invisible in every green signal this project had.
// `package.json` pinned `@patternfly/react-styles` at 6.6.0 while
// `@patternfly/patternfly`, `react-core`, `react-icons` and `react-table` sat
// at 6.1.0. Those two packages are not interchangeable halves:
// `@patternfly/patternfly` ships the `--pf-t--*` DESIGN TOKENS (this plugin
// loads them via `patternfly-base.css` + `patternfly-addons.css` in
// index.tsx), while `@patternfly/react-styles` ships the per-component CSS
// that every barrel import under `src/` side-effect-pulls in. So the rendered
// page ran 6.6.0 component CSS against a 6.1.0 token set.
//
// A CSS custom property that resolves to nothing is not an error -- the
// declaration referencing it just becomes invalid at computed-value time and
// silently falls back to the property's initial value. So nothing failed:
// tsc, eslint and all 215 tests were green, and the page rendered. Measured
// on the built `dist/index.css`, 50 distinct tokens were referenced with no
// fallback and never defined, across 264 references -- borders, radii, focus
// rings, glass backgrounds and brand accents. Aligning every package on 6.6.0
// took that to 2 references, both dangling upstream in PatternFly 6.6.0
// itself (`--pf-t--global--font--size--body` in AboutModalBox, and
// `--pf-t--global--spacer--gap--horizontal` behind the `m-info` form-label
// modifier), neither of which this plugin renders.
//
// The symptom that exposed it, found in a real browser rather than in code:
// `.pf-v6-c-page__main-section` computed `padding-inline: 0px` instead of
// 20px, because 6.6.0's page.css computes it as
// `calc(var(--pf-v6-c-page__main-section--PaddingInlineStart) - var(--pf-v6-c-page__main-container--BorderInlineStartWidth))`
// and that second variable chains to `--pf-t--global--border--width--main--default`,
// a token 6.1.0 does not define. Cockpit 356's own stylesheet was the
// tie-breaker for WHICH way to align: it contains the
// `.pf-v6-c-page.pf-m-no-sidebar` selector and no
// `--pf-v6-c-page__main-container--BorderWidth`, both of which are 6.6-era,
// so Cockpit itself is on that release and aligning up (not down) is what
// matches the host shell. Aligning down would also have silently broken
// the sidebar fix, since `pf-m-no-sidebar` does not exist in 6.1.0's page.css.
//
// This asserts the invariant rather than a specific version, so a future
// coordinated PatternFly bump passes untouched and only a partial one --
// exactly the shape of this defect, and the shape a grouped Dependabot update
// produces when one package in the group is held back -- fails.
//
// It has since caught a second, different route to the same split, which is
// why `package.json` carries an `overrides` entry for
// `@patternfly/react-tokens`. The five PatternFly packages this project
// depends on directly are pinned to exact versions, but `react-tokens` is
// pulled in transitively by `react-core` at `^6.6.0`, and `package-lock.json`
// is gitignored here (cockpit/.gitignore, inherited from the starter kit,
// whose Makefile deletes the lock on purpose to stay on latest). So a release
// build resolved it fresh months after the pins were chosen and picked up
// 6.6.1 against a 6.6.0 token set. It is not a harmless mismatch: the module
// is in the shipped bundle, listed in `runtime-npm-modules.txt`. The override
// pins the transitive one to match, and this test is what would notice if a
// seventh package ever arrives by the same route.
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";

const patternflyDir = path.join(
    path.dirname(fileURLToPath(import.meta.url)),
    "..",
    "node_modules",
    "@patternfly",
);

test("every installed @patternfly package is on the same release", () => {
    const installed = fs.readdirSync(patternflyDir, { withFileTypes: true })
            .filter(entry => entry.isDirectory())
            .map(entry => {
                const manifest = path.join(patternflyDir, entry.name, "package.json");
                return {
                    name: `@patternfly/${entry.name}`,
                    version: JSON.parse(fs.readFileSync(manifest, "utf8")).version as string,
                };
            })
            .sort((a, b) => a.name.localeCompare(b.name));

    // Guards the guard: if the directory scan ever finds nothing (a moved
    // node_modules, a renamed scope), an empty list would make the version
    // check below vacuously true and this test would go on passing while
    // checking nothing at all.
    assert.ok(
        installed.length >= 5,
        `expected to find the PatternFly packages under ${patternflyDir}, found ${installed.length}: ${JSON.stringify(installed)}`,
    );

    const versions = [...new Set(installed.map(pkg => pkg.version))];
    assert.equal(
        versions.length,
        1,
        `@patternfly packages are split across ${versions.length} releases (${versions.join(", ")}). ` +
        "`@patternfly/patternfly` ships the --pf-t--* tokens and `@patternfly/react-styles` ships the " +
        "component CSS that consumes them; when they disagree, every token the newer CSS references and " +
        "the older token set omits resolves to nothing, and the declaration using it silently falls back " +
        "to its initial value with no build or test error. Installed: " +
        installed.map(pkg => `${pkg.name}@${pkg.version}`).join(", "),
    );
});
