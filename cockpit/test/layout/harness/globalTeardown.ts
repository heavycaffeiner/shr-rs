/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Merges what the workers wrote into one artifact and prints the two figures
 * no single run can see: which exclusions never fired, and how far off the 4px
 * grid the page actually is.
 */

import fs from "node:fs";
import path from "node:path";

import { exclusions } from "../exclusions.ts";
import type { CheckId } from "../audit.ts";
import { REPORT_DIR, readRunReports } from "./report.ts";

export default (): void => {
    const runs = readRunReports();
    if (runs.length === 0)
        return;

    const byCheck = new Map<CheckId, number>();
    const used = new Set<number>();
    const totals = { elements: 0, suppressed: 0 };

    for (const run of runs) {
        for (const violation of run.result.violations)
            byCheck.set(violation.check, (byCheck.get(violation.check) ?? 0) + 1);
        for (const index of run.result.usedExclusions)
            used.add(index);
        for (const key of Object.keys(totals) as (keyof typeof totals)[])
            totals[key] += run.result.stats[key];
    }

    const stale = exclusions
            .map((entry, index) => ({ entry, index }))
            .filter(({ index }) => !used.has(index));

    const summary = {
        runs: runs.length,
        totals,
        violationsByCheck: Object.fromEntries([...byCheck].sort()),
        staleExclusions: stale.map(({ entry, index }) => ({ index, selector: entry.selector, checks: entry.checks })),
        violations: runs.flatMap(run => run.result.violations.map(violation => ({ run: run.run, ...violation }))),
    };

    fs.mkdirSync(REPORT_DIR, { recursive: true });
    fs.writeFileSync(path.join(REPORT_DIR, "violations.json"), `${JSON.stringify(summary, null, 2)}\n`, "utf8");

    const byCheckLine = [...byCheck]
            .sort()
            .map(([check, count]) => `  ${check.padEnd(4)} ${count}`);

    process.stdout.write([
        "",
        `layout audit: ${runs.length} runs, ${totals.elements} elements measured, ${summary.violations.length} violations`,
        ...byCheckLine,
        `  exclusions suppressed: ${totals.suppressed}`,
        `  report:                ${path.join(REPORT_DIR, "violations.json")}`,
        "",
    ].join("\n"));

    // An exclusion that suppresses nothing is either a defect somebody fixed
    // or a selector that stopped matching, and both of those turn the list
    // into scenery.
    if (stale.length > 0) {
        const names = stale.map(({ entry }) => `${entry.selector} (${entry.checks.join(", ")})`).join(", ");
        throw new Error(`exclusions.ts has entries that suppressed nothing across the whole matrix: ${names}`);
    }
};
