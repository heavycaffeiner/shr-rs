/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Playwright runs the matrix across several worker processes, so no single
 * process sees every result. Each run writes its own file and the global
 * teardown merges them; that is the whole reason for the two directories.
 */

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

import type { AuditResult } from "../audit.ts";

export const REPORT_DIR = path.resolve(fileURLToPath(new URL("../report", import.meta.url)));
export const RUNS_DIR = path.join(REPORT_DIR, "runs");

export interface RunReport {
    run: string;
    view: string;
    locale: string;
    theme: string;
    width: number;
    fixture: string;
    result: AuditResult;
}

export const writeRunReport = (report: RunReport): void => {
    fs.mkdirSync(RUNS_DIR, { recursive: true });
    // The run id reaches this from a spec title, so it is not a safe filename.
    const safe = report.run.replace(/[^a-zA-Z0-9._-]+/g, "_");
    fs.writeFileSync(path.join(RUNS_DIR, `${safe}.json`), `${JSON.stringify(report, null, 2)}\n`, "utf8");
};

export const readRunReports = (): RunReport[] => {
    if (!fs.existsSync(RUNS_DIR))
        return [];
    return fs.readdirSync(RUNS_DIR)
            .filter(name => name.endsWith(".json"))
            .map(name => JSON.parse(fs.readFileSync(path.join(RUNS_DIR, name), "utf8")) as RunReport)
            .sort((a, b) => a.run.localeCompare(b.run));
};
