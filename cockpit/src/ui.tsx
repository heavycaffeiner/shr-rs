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

/* No wrapping class accompanies `MONO`. That was tried and removed: a
 * `pf-v6-u-text-break-word` variant for long unbreakable values (`fs_uuid`,
 * `md_uuid`, a `by-id` name) turned out to change nothing measurable, because
 * PatternFly already sets `overflow-wrap: break-word` on the table cells and
 * description-list descriptions those values live in. Measured at 390px with
 * and without it: identical, a 44-character `by-id` name wrapping to 138px
 * inside a 246px parent either way, zero elements past the viewport in both.
 *
 * It was not free either. `word-break: break-word` is defined as
 * `overflow-wrap: anywhere`, and `anywhere` also drops min-content width to a
 * single character; while it sat on `MONO` itself the band table's auto layout
 * squeezed its short columns to nothing, rendering `band0` as "band" / "0" and
 * `RAID5` as "RAID" / "5" at 1280px. */

/** Goes on every `ActionList` in this plugin.
 *
 * `ActionList` is the one layout here that genuinely cannot wrap on its own.
 * PatternFly's `Flex` defaults to `--pf-v6-l-flex--FlexWrap: wrap` and `Split`
 * at least offers `isWrappable`, but `.pf-v6-c-action-list` is a bare
 * `display: flex` with no wrap declaration and no modifier for one. Its only
 * layout prop is the beta `isVertical`, which would stack buttons at every
 * width including desktop.
 *
 * So the wrapping comes from `patternfly-addons.css`'s own utility rather than
 * from a rule this package writes. It is inert wherever the buttons already
 * fit, which is every dialog at desktop width.
 *
 * What it fixes was measured on the pre-change bundle at 390px: `OperationsPanel`'s
 * six-button row ran to x=1057 in a 390-pixel viewport, and the four buttons past
 * the fold ("Replace a disk", "Create a snapshot", "Change compression" and
 * "Destroy") were clipped by the card rather than merely pushed offscreen. The
 * document's own scrollWidth never grew, so there was no sideways scrollbar to
 * hint at them: they were simply unreachable. */
export const ACTION_ROW = "pf-v6-u-flex-wrap";

/** Wraps the text passed as a dialog's `ModalHeader title`.
 *
 * PatternFly gives both `.pf-v6-c-modal-box__title` and its inner
 * `__title-text` `white-space: nowrap` with `text-overflow: ellipsis`, so a
 * modal title is a single line by design. That is fine on a desktop and not
 * fine here: every one of this plugin's dialog titles ends in the group it is
 * about, so the phone-width ellipsis eats exactly the identifying part.
 * Measured at 390px, "Change compression for group \"shr1\"" ended at
 * `...for grou` -- and that dialog's body never names the group again, so the
 * operator had nothing left to tell two groups apart by.
 *
 * The utility goes on a span INSIDE the title rather than on the title itself:
 * `white-space: normal` on a descendant is enough to let the text wrap, while
 * the title element keeps the rest of PatternFly's header styling. The title
 * box has no fixed height, so it simply grows to two lines. */
export const TITLE_WRAP = "pf-v6-u-text-wrap";

/** Goes on every `<fieldset>` in this plugin.
 *
 * A `<fieldset>` carries the UA default `min-inline-size: min-content`, which
 * no other element does and which means it refuses to shrink below its widest
 * child no matter how narrow its container is. Measured in the expand dialog
 * at 390px: the fieldset sat at 344px inside a 326px body and its content was
 * clipped by the modal, so the disk list ran under the dialog's edge.
 *
 * `pf-v6-u-min-width` is PatternFly's `min-width: 0 !important`, which is the
 * standard remedy and the reason the utility exists. */
export const FIELDSET_SHRINK = "pf-v6-u-min-width";

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
