/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

import fs from "node:fs";

import { REPORT_DIR, RUNS_DIR } from "./report.ts";

/* Clearing this is not tidiness. A run that crashes before it writes leaves
   yesterday's file behind, and the teardown would merge it and report a
   matrix that never executed. */
export default (): void => {
    fs.rmSync(RUNS_DIR, { recursive: true, force: true });
    fs.mkdirSync(RUNS_DIR, { recursive: true });
    fs.rmSync(`${REPORT_DIR}/violations.json`, { force: true });
};
