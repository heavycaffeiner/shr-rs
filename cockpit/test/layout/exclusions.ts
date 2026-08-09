/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Places where a check's finding is not a defect.
 *
 * Every entry carries a reason, and `harness/run.ts` fails the run on an entry
 * that suppressed nothing. Both rules exist for the same purpose: an exclusion
 * list is the one part of an audit that can quietly turn it green, so it is
 * kept small enough to read and it cannot rot unnoticed.
 *
 * `selector` is matched with `closest()`, so an entry covers an element and
 * everything under it.
 */

import type { Exclusion } from "./audit.ts";

export const exclusions: Exclusion[] = [
    {
        selector: ".pf-v6-c-page__main",
        checks: ["B6"],
        reason: "the page's own scroll container: content taller than the viewport is what it is for",
    },
    {
        selector: ".pf-v6-c-action-list__item",
        checks: ["P3"],
        reason: "PatternFly 6.6.0's ActionList emits this class and ships no rule for it: action-list.css styles the group, not the item",
    },
    {
        selector: ".pf-v6-c-expandable-section__toggle",
        checks: ["P3"],
        reason: "PatternFly 6.6.0's ExpandableSection emits this class and ships no rule for it: expandable-section.css styles only __toggle-icon",
    },
];
