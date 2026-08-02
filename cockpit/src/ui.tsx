/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * The handful of primitives every panel and dialog needs, expressed in
 * PatternFly rather than in a private stylesheet.
 *
 * This file exists because the plugin used to ship its own design system
 * (`app.scss`: a hand-copied PatternFly 4/5 palette, its own `.chip`,
 * `.status-badge`, `.cell-sub`, `.mono`, `.modal-overlay`, ...). Those values
 * drifted from the Cockpit shell they sat next to -- light background
 * `#f4f4f4` against the shell's `#f2f2f2`, dark foreground `#f0f0f0` against
 * the shell's `#fff` -- and none of PatternFly 6's ~1000 design tokens were
 * present at all. Everything here is either a real PatternFly component or a
 * PatternFly utility class, so the plugin inherits the shell's tokens (and
 * its light/dark switch, see `darkTheme.ts`) instead of re-deciding them.
 *
 * Keep this file small. Anything that is a plain PatternFly component at the
 * call site should be imported from `@patternfly/react-core` directly, not
 * re-wrapped here.
 */

import React from "react";

import { Card, CardBody, CardTitle, Content, Label } from "@patternfly/react-core";

/** Device nodes, UUIDs, argv previews -- anything where character alignment
 * carries meaning. PatternFly ships Red Hat Mono; `app.scss` used to name the
 * family itself. */
export const MONO = "pf-v6-u-font-family-monospace";

export interface Tone {
    label: string;
    tone: "good" | "warning" | "neutral";
}

/** Maps this project's three-way tone onto PatternFly's `Label` colors. The
 * dot that `.status-badge__dot` used to draw by hand is `Label`'s own
 * `status` rendering. */
export const Badge = ({ tone }: { tone: Tone }) => {
    // `status` is spread rather than passed as `status={... : undefined}`:
    // `exactOptionalPropertyTypes` treats an explicit `undefined` as a type
    // error, and a neutral tone genuinely has no PatternFly status.
    const status = tone.tone === "good"
        ? { status: "success" as const }
        : tone.tone === "warning" ? { status: "warning" as const } : {};
    return (
        <Label
            isCompact
            color={tone.tone === "good" ? "green" : tone.tone === "warning" ? "orange" : "grey"}
            {...status}
        >
            {tone.label}
        </Label>
    );
};

/** A secondary line under a table cell's primary value (`.cell-sub`). Block
 * so consecutive ones stack, which is what every caller relies on. */
export const CellSub = ({ children, className }: { children: React.ReactNode; className?: string }) => (
    <div className={`pf-v6-u-font-size-sm pf-v6-u-text-color-subtle${className ? ` ${className}` : ""}`}>
        {children}
    </div>
);

/** "no value here", as opposed to a value that happens to be empty
 * (`.muted`). */
export const Muted = ({ children }: { children: React.ReactNode }) => (
    <span className="pf-v6-u-text-color-subtle">{children}</span>
);

/** A short inline token -- a device id, a mode name, a member name
 * (`.chip`). `Label` is PatternFly's own name for this. */
export const Chip = (
    { children, color, title }: {
        children: React.ReactNode;
        color?: "grey" | "red" | "orange" | "blue" | "purple";
        title?: string;
    },
) => (
    <Label isCompact color={color ?? "grey"} className={MONO} title={title}>
        {children}
    </Label>
);

/** Explanatory small print under a panel (`.capacity-caveat`). */
export const Caveat = ({ children }: { children: React.ReactNode }) => (
    <Content component="small" className="pf-v6-u-text-color-subtle">{children}</Content>
);

/** One tile of the dashboard's summary row (`.metric-card`). Both the
 * top-level status row in `app.tsx` and the capacity row in `panels.tsx`
 * render these, so the shape lives here rather than in either of them.
 *
 * `value` is a node, not a string: several call sites put a `Badge` there
 * instead of a number, which the old markup handled by having two different
 * `.metric-card__value` elements (a `<strong>` and a `<span>`). */
export const MetricCard = (
    { label, value, sub }: { label: string; value: React.ReactNode; sub?: React.ReactNode },
) => (
    <Card isCompact isPlain component="div">
        <CardTitle className="pf-v6-u-font-size-sm pf-v6-u-text-color-subtle">{label}</CardTitle>
        <CardBody>
            <div className="pf-v6-u-font-size-xl">{value}</div>
            {sub !== undefined && sub !== null && sub !== "" && (
                <div className="pf-v6-u-font-size-sm pf-v6-u-text-color-subtle pf-v6-u-mt-xs">{sub}</div>
            )}
        </CardBody>
    </Card>
);
