/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Everything below `auditPage` runs inside the browser, handed to
 * `page.evaluate`. Playwright serializes the function itself and nothing
 * around it, so the body is deliberately self-contained: a helper hoisted to
 * module scope would be `undefined` by the time the page called it.
 *
 * Five preconditions run first. They exist because every check further down
 * is a number compared against 4, and a page that rendered without its
 * stylesheet, at a fractional device pixel ratio, or with the wrong catalogue
 * produces numbers that are internally consistent and mean nothing. A green
 * run has to be evidence, so the run refuses to report one until it has
 * proved it measured the real page.
 */

/* B4 is absent on purpose. It snapped realized element bounds to the 4px
 * lattice, and measured across 52 runs it rejected 54% of position edges and
 * 82% of sizes: line boxes are 20.8px because the type scale is 16px at 1.3,
 * flex remainders are whatever is left over, and every box after a line of text
 * inherits its fraction. None of that is a value this package writes, and B1
 * already checks the ones it does. */
export type CheckId =
    | "P1" | "P2" | "P3" | "P4" | "P5"
    | "B1" | "B2" | "B3" | "B5" | "B6" | "B7a" | "B7b" | "B7c";

export interface Violation {
    check: CheckId;
    element: string;
    detail: string;
}

export interface Exclusion {
    /** Matched with `closest()`, so an entry covers an element and its
     * subtree. */
    selector: string;
    checks: CheckId[];
    /** Why this is not a defect. An entry without one is not reviewable. */
    reason: string;
}

export interface AuditOptions {
    locale: "en" | "ko";
    exclusions: Exclusion[];
}

export interface AuditResult {
    violations: Violation[];
    /** Indices into `options.exclusions` that actually suppressed something.
     * An entry that never fires is stale and the spec fails on it. */
    usedExclusions: number[];
    stats: {
        elements: number;
        suppressed: number;
    };
}

export const auditPage = (options: AuditOptions): AuditResult => {
    const GRID = 4;
    const EPS = 0.5;
    /* --pf-t--global--spacer--100..800, resolved at a 16px root. There is no
       12px step, which is why 12 is a violation rather than a rounding. */
    const SCALE = [0, 4, 8, 16, 24, 32, 48, 64, 80];
    const MIN_TARGET = 24;

    const violations: Violation[] = [];
    const usedExclusions = new Set<number>();
    let suppressed = 0;

    const describe = (el: Element): string => {
        const parts: string[] = [];
        let node: Element | null = el;
        while (node && node !== document.documentElement && parts.length < 4) {
            let text = node.tagName.toLowerCase();
            if (node.id) {
                parts.unshift(`${text}#${node.id}`);
                break;
            }
            const classes = Array.from(node.classList).slice(0, 3)
                    .join(".");
            if (classes)
                text += `.${classes}`;
            parts.unshift(text);
            node = node.parentElement;
        }
        return parts.join(" > ");
    };

    const report = (check: CheckId, el: Element | null, detail: string): void => {
        if (el) {
            for (let i = 0; i < options.exclusions.length; i++) {
                const entry = options.exclusions[i];
                if (entry.checks.includes(check) && el.closest(entry.selector)) {
                    usedExclusions.add(i);
                    suppressed++;
                    return;
                }
            }
        }
        violations.push({ check, element: el ? describe(el) : "(document)", detail });
    };

    const round2 = (value: number): number => Math.round(value * 100) / 100;
    const onScale = (value: number): boolean =>
        SCALE.some(step => Math.abs(Math.abs(value) - step) <= EPS);

    // --- P5 first: it appends a probe, and everything after this reads the
    // DOM as it stands. ---------------------------------------------------
    if (options.locale === "ko") {
        const probe = document.createElement("span");
        probe.style.cssText = "position:absolute;left:-9999px;top:0;visibility:hidden;white-space:pre;font:inherit;";
        document.body.appendChild(probe);
        const advance = (text: string): number => {
            probe.textContent = text.repeat(10);
            return probe.getBoundingClientRect().width;
        };
        const hangul = advance("가");
        // U+E000 is private use: no font in a normal install has a glyph for
        // it, so its advance is the missing-glyph box. Hangul matching it
        // means the CJK font is absent and the page is rendering tofu, which
        // would pass every other check in this file while showing nothing a
        // reader could use.
        const tofu = advance("\uE000");
        probe.remove();
        if (Math.abs(hangul - tofu) < 1)
            report("P5", null, `Hangul renders at the missing-glyph advance (${round2(hangul)}px vs ${round2(tofu)}px): no CJK font is installed, so this run measured tofu`);
    }

    // --- P1, P2 ----------------------------------------------------------
    const rootFontSize = getComputedStyle(document.documentElement).fontSize;
    if (rootFontSize !== "16px")
        report("P1", null, `root font-size is ${rootFontSize}, not 16px: the rem-based spacer scale no longer lands on 4px multiples`);

    if (window.devicePixelRatio !== 1)
        report("P2", null, `devicePixelRatio is ${window.devicePixelRatio}, not 1: layout is snapped to device pixels and every measurement below is off-grid by construction`);

    // --- P3: the stylesheet really applied -------------------------------
    const styledClasses = new Set<string>();
    const collectSelectors = (rules: CSSRuleList): void => {
        for (const rule of Array.from(rules)) {
            const selector = (rule as CSSStyleRule).selectorText;
            if (typeof selector === "string") {
                for (const match of selector.matchAll(/\.((?:pf-v6|pf-m)[\w-]*)/g))
                    styledClasses.add(match[1]);
            }
            const nested = (rule as CSSGroupingRule).cssRules;
            if (nested)
                collectSelectors(nested);
        }
    };
    for (const sheet of Array.from(document.styleSheets)) {
        try {
            collectSelectors(sheet.cssRules);
        } catch {
            report("P3", null, `a stylesheet (${sheet.href ?? "inline"}) could not be read, so which classes are styled is unknown`);
        }
    }
    // Reported against the first element carrying the class rather than the
    // document, so a class a dependency emits without styling can be excluded
    // by selector like any other finding.
    const unstyled = new Map<string, Element>();
    for (const el of Array.from(document.querySelectorAll("[class]"))) {
        for (const cls of Array.from(el.classList)) {
            if ((cls.startsWith("pf-v6-") || cls.startsWith("pf-m-")) && !styledClasses.has(cls) && !unstyled.has(cls))
                unstyled.set(cls, el);
        }
    }
    for (const [cls, el] of unstyled)
        report("P3", el, `\`${cls}\` is on an element but no rule in the loaded stylesheets names it`);

    // --- P4: the catalogue really applied, in both directions -------------
    const HANGUL = /[ᄀ-ᇿ㄰-㆏ꥠ-꥿가-힯]/;
    const bodyText = document.body.innerText ?? "";
    if (document.documentElement.lang !== options.locale)
        report("P4", null, `<html lang> is "${document.documentElement.lang}", not "${options.locale}"`);
    if (options.locale === "ko" && !HANGUL.test(bodyText))
        report("P4", null, "the Korean run rendered no Hangul at all: po.ko.js did not apply");
    if (options.locale === "en" && HANGUL.test(bodyText))
        report("P4", null, "the English run rendered Hangul: a Korean catalogue leaked into an English session");
    // A missing catalogue leaves this package's dotted msgids on screen, and
    // those are short ASCII strings that would sail through the two checks
    // above on an English run.
    const rawKey = /(?:^|\s)(?:app|panels|dialogs|wizard|actions|common|model)\.[a-z][A-Za-z]*(?:\.[A-Za-z]+)*/.exec(bodyText);
    if (rawKey)
        report("P4", null, `an untranslated message key is on screen ("${rawKey[0].trim()}"): the catalogue did not cover the page`);

    // --- Collect the elements every B check works from --------------------
    interface Box { el: Element; style: CSSStyleDeclaration; rect: DOMRect }

    const records: Box[] = [];
    const byElement = new Map<Element, Box>();
    for (const el of Array.from(document.body.querySelectorAll("*"))) {
        // An <svg>'s internals have their own coordinate system and are not
        // laid out by CSS box rules; `closest` catches the root too.
        if (el.closest("svg"))
            continue;
        const tag = el.tagName.toLowerCase();
        if (tag === "script" || tag === "style" || tag === "template" || tag === "link" || tag === "br")
            continue;
        const style = getComputedStyle(el);
        if (style.display === "none" || style.visibility === "hidden" || style.opacity === "0")
            continue;
        const rect = el.getBoundingClientRect();
        if (rect.width <= EPS || rect.height <= EPS)
            continue;
        const record = { el, style, rect };
        records.push(record);
        byElement.set(el, record);
    }

    const inFlow = (record: Box): boolean =>
        record.style.position !== "absolute" &&
        record.style.position !== "fixed" &&
        record.style.float === "none";

    const childrenOf = (el: Element): Box[] => {
        const kids: Box[] = [];
        for (const child of Array.from(el.children)) {
            const record = byElement.get(child);
            if (record && inFlow(record))
                kids.push(record);
        }
        return kids;
    };

    const BLOCK_LEVEL = ["block", "flow-root", "flex", "grid", "table", "list-item"];
    const isBlockLevel = (record: Box): boolean => BLOCK_LEVEL.includes(record.style.display);

    /* Splits a row's children into the visual lines a wrapping flex container
       actually produced. Without this, a wrapped action row reads as one line
       whose items are "misaligned" by a full row height. */
    const intoLines = (kids: Box[]): Box[][] => {
        const sorted = [...kids].sort((a, b) => a.rect.top - b.rect.top);
        const lines: Box[][] = [];
        let bottom = -Infinity;
        for (const kid of sorted) {
            if (lines.length === 0 || kid.rect.top >= bottom - EPS) {
                lines.push([kid]);
                bottom = kid.rect.bottom;
            } else {
                lines[lines.length - 1].push(kid);
                bottom = Math.max(bottom, kid.rect.bottom);
            }
        }
        for (const line of lines)
            line.sort((a, b) => a.rect.left - b.rect.left);
        return lines;
    };

    /** The value most of the group agrees on, so the report names the outlier
     * rather than the majority. */
    const modeOf = (values: number[]): number => {
        let best = values[0];
        let bestCount = 0;
        for (const value of values) {
            const count = values.filter(other => Math.abs(other - value) <= EPS).length;
            if (count > bestCount) {
                best = value;
                bestCount = count;
            }
        }
        return best;
    };

    // --- B1: spacing this package specified, on the spacer scale ----------
    const SIDES: Record<string, string[]> = {
        "": ["Top", "Right", "Bottom", "Left"],
        t: ["Top"],
        r: ["Right"],
        b: ["Bottom"],
        l: ["Left"],
        x: ["Left", "Right"],
        y: ["Top", "Bottom"],
    };
    const UTILITY = /^pf-v6-u-(m|p)(t|r|b|l|x|y)?-([a-z0-9]+)(?:-on-[a-z0-9]+)?$/;

    for (const { el, style } of records) {
        for (const cls of Array.from(el.classList)) {
            const match = UTILITY.exec(cls);
            if (!match)
                continue;
            // `auto` resolves to a centring distance, which is a used value
            // rather than a value this package chose.
            if (match[3] === "auto")
                continue;
            const property = match[1] === "m" ? "margin" : "padding";
            for (const side of SIDES[match[2] ?? ""]) {
                const raw = style.getPropertyValue(`${property}-${side.toLowerCase()}`);
                const px = Number.parseFloat(raw);
                if (!Number.isFinite(px) || onScale(px))
                    continue;
                report("B1", el, `\`${cls}\` realizes ${property}-${side.toLowerCase()} as ${round2(px)}px, which is not on the spacer scale (${SCALE.join(", ")})`);
            }
        }
        // A `pf-v6-l-*` element is a layout this package instantiated and
        // whose gap it chose through `hasGutter`/`spaceItems`/`gap`.
        if (!Array.from(el.classList).some(cls => cls.startsWith("pf-v6-l-")))
            continue;
        for (const property of ["column-gap", "row-gap"]) {
            const raw = style.getPropertyValue(property);
            const px = Number.parseFloat(raw);
            if (!Number.isFinite(px) || onScale(px))
                continue;
            report("B1", el, `${property} is ${round2(px)}px, which is not on the spacer scale (${SCALE.join(", ")})`);
        }
    }

    // --- B2: vertically stacked siblings share their inline edges ---------
    for (const record of records) {
        const { el, style } = record;
        const isBlockContainer = style.display === "block" || style.display === "flow-root";
        const isColumnFlex = (style.display === "flex" || style.display === "inline-flex") &&
            style.flexDirection.startsWith("column");
        if (!isBlockContainer && !isColumnFlex)
            continue;

        const kids = childrenOf(el).filter(isBlockLevel);
        if (kids.length < 2)
            continue;

        // A child with a margin on the edge being compared has been pulled off
        // that edge deliberately, so it is not evidence of a misalignment. The
        // margin itself is B1's to judge.
        const flush = (kid: Box, side: "marginLeft" | "marginRight"): boolean =>
            Math.abs(parseFloat(kid.style[side]) || 0) <= EPS;

        const aligned = kids.filter(kid => flush(kid, "marginLeft"));
        if (aligned.length >= 2) {
            const referenceLeft = modeOf(aligned.map(kid => kid.rect.left));
            for (const kid of aligned) {
                if (Math.abs(kid.rect.left - referenceLeft) > EPS)
                    report("B2", kid.el, `starts at x=${round2(kid.rect.left)} while its stacked siblings start at x=${round2(referenceLeft)}`);
            }
        }

        // The trailing edge only has to agree where the container stretches
        // its children to fill the cross axis. A block container's children
        // may legitimately size to their own content.
        if (!isColumnFlex || !["stretch", "normal"].includes(style.alignItems))
            continue;
        const stretched = kids.filter(kid =>
            ["auto", "stretch", "normal", ""].includes(kid.style.alignSelf) && flush(kid, "marginRight"));
        if (stretched.length < 2)
            continue;
        const referenceRight = modeOf(stretched.map(kid => kid.rect.right));
        for (const kid of stretched) {
            if (Math.abs(kid.rect.right - referenceRight) > EPS)
                report("B2", kid.el, `ends at x=${round2(kid.rect.right)} while its stretched siblings end at x=${round2(referenceRight)}`);
        }
    }

    // --- B3: cross-axis alignment matches what the container declared -----
    for (const record of records) {
        const { el, style } = record;
        if (style.display !== "flex" && style.display !== "inline-flex")
            continue;
        if (!style.flexDirection.startsWith("row"))
            continue;
        const align = style.alignItems;
        if (align === "baseline" || align === "last baseline")
            continue;

        for (const line of intoLines(childrenOf(el))) {
            const kids = line.filter(kid => ["auto", "stretch", "normal", ""].includes(kid.style.alignSelf));
            if (kids.length < 2)
                continue;
            const measure = (kid: Box): number => {
                if (align === "center")
                    return kid.rect.top + kid.rect.height / 2;
                if (align === "flex-end" || align === "end")
                    return kid.rect.bottom;
                if (align === "stretch" || align === "normal")
                    return kid.rect.height;
                return kid.rect.top;
            };
            const label = align === "center"
                ? "centre"
                : align === "flex-end" || align === "end"
                    ? "bottom edge"
                    : align === "stretch" || align === "normal" ? "height" : "top edge";
            const values = kids.map(measure);
            const reference = modeOf(values);
            for (let i = 0; i < kids.length; i++) {
                if (Math.abs(values[i] - reference) > EPS)
                    report("B3", kids[i].el, `align-items: ${align} on the parent, but its ${label} is ${round2(values[i])} against ${round2(reference)} for the rest of the line`);
            }
        }
    }

    // --- B5: baselines, where the container asked for them ----------------
    const firstLineRect = (el: Element): DOMRect | null => {
        const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
        let node = walker.nextNode();
        while (node) {
            if ((node.textContent ?? "").trim()) {
                const range = document.createRange();
                range.selectNodeContents(node);
                const rects = range.getClientRects();
                if (rects.length > 0)
                    return rects[0];
            }
            node = walker.nextNode();
        }
        return null;
    };
    const scriptOf = (el: Element): string => {
        const text = el.textContent ?? "";
        return HANGUL.test(text) ? "hangul" : "latin";
    };

    for (const { el, style } of records) {
        if (style.display !== "flex" && style.display !== "inline-flex")
            continue;
        if (!style.flexDirection.startsWith("row"))
            continue;
        if (style.alignItems !== "baseline")
            continue;

        for (const line of intoLines(childrenOf(el))) {
            const texts = line
                    .map(kid => ({ kid, rect: firstLineRect(kid.el) }))
                    .filter((entry): entry is { kid: Box; rect: DOMRect } => entry.rect !== null);
            if (texts.length < 2)
                continue;
            // A computed `font-family` is the declared list, not the face the
            // browser picked, so it cannot tell a Hangul run from a Latin one
            // even when both read "Red Hat Text, ...". Comparing the scripts
            // as well is what keeps this from measuring a fallback font's
            // metrics and calling them a misalignment.
            const families = new Set(texts.map(entry => entry.kid.style.fontFamily));
            const scripts = new Set(texts.map(entry => scriptOf(entry.kid.el)));
            if (families.size > 1 || scripts.size > 1)
                continue;
            const bottoms = texts.map(entry => entry.rect.bottom);
            const reference = modeOf(bottoms);
            for (let i = 0; i < texts.length; i++) {
                if (Math.abs(bottoms[i] - reference) > EPS)
                    report("B5", texts[i].kid.el, `align-items: baseline on the parent, but its first line sits at y=${round2(bottoms[i])} against ${round2(reference)} for the rest of the line`);
            }
        }
    }

    // --- B6: nothing is clipped or pushed out of the viewport -------------
    const viewportWidth = document.documentElement.clientWidth;
    for (const { el, style, rect } of records) {
        // An ellipsis IS the overflow: the element is declaring that it will
        // truncate, so scrollWidth exceeding clientWidth is the design.
        const truncates = style.textOverflow === "ellipsis";
        // scrollWidth/scrollHeight are integers, so 0.5px will not do here.
        if (!truncates && style.overflowX !== "visible" && el.scrollWidth > el.clientWidth + 1)
            report("B6", el, `content is ${el.scrollWidth}px wide inside a ${el.clientWidth}px box and is clipped horizontally`);
        if (style.overflowY !== "visible" && el.scrollHeight > el.clientHeight + 1)
            report("B6", el, `content is ${el.scrollHeight}px tall inside a ${el.clientHeight}px box and is clipped vertically`);
        if (rect.right > viewportWidth + EPS)
            report("B6", el, `extends to x=${round2(rect.right)} past the ${viewportWidth}px viewport`);
        if (rect.left < -EPS)
            report("B6", el, `starts at x=${round2(rect.left)}, off the left of the viewport`);
    }

    // --- B7a: a box that paints its own edge insets its content by 0 or 4+ -
    const paintsEdge = (style: CSSStyleDeclaration): boolean => {
        const background = style.backgroundColor;
        const opaque = background !== "transparent" && !/^rgba\([^)]*,\s*0\)$/.test(background);
        const bordered = ["Top", "Right", "Bottom", "Left"].some(side =>
            Number.parseFloat(style.getPropertyValue(`border-${side.toLowerCase()}-width`)) > 0 &&
            style.getPropertyValue(`border-${side.toLowerCase()}-style`) !== "none");
        return opaque || bordered;
    };

    const contentExtent = (el: Element): DOMRect | null => {
        let left = Infinity;
        let top = Infinity;
        let right = -Infinity;
        let bottom = -Infinity;
        let found = false;
        const absorb = (rect: DOMRect): void => {
            if (rect.width <= EPS && rect.height <= EPS)
                return;
            found = true;
            left = Math.min(left, rect.left);
            top = Math.min(top, rect.top);
            right = Math.max(right, rect.right);
            bottom = Math.max(bottom, rect.bottom);
        };
        for (const child of Array.from(el.children)) {
            const record = byElement.get(child);
            if (record && inFlow(record))
                absorb(record.rect);
        }
        // Direct text children have no box of their own, and on a card body
        // they are usually the only content there is.
        for (const node of Array.from(el.childNodes)) {
            if (node.nodeType !== Node.TEXT_NODE || !(node.textContent ?? "").trim())
                continue;
            const range = document.createRange();
            range.selectNodeContents(node);
            absorb(range.getBoundingClientRect());
        }
        return found ? new DOMRect(left, top, right - left, bottom - top) : null;
    };

    for (const { el, style, rect } of records) {
        if (!paintsEdge(style))
            continue;
        const extent = contentExtent(el);
        if (!extent)
            continue;
        const border = (side: string): number =>
            Number.parseFloat(style.getPropertyValue(`border-${side}-width`)) || 0;
        const insets: [string, number][] = [
            ["start", extent.left - (rect.left + border("left"))],
            ["end", (rect.right - border("right")) - extent.right],
            ["top", extent.top - (rect.top + border("top"))],
            ["bottom", (rect.bottom - border("bottom")) - extent.bottom],
        ];
        for (const [side, inset] of insets) {
            // Negative is content spilling out, which is B6's finding, not a
            // padding one. Zero is a deliberate full-bleed edge.
            if (inset > EPS && inset < GRID - EPS)
                report("B7a", el, `paints its own edge but insets its content by ${round2(inset)}px on the ${side}: either flush at 0 or at least ${GRID}px`);
        }
    }

    // --- B7b: neighbouring siblings sit flush or at least 4px apart -------
    const reportGap = (previous: Box, next: Box, gap: number, axis: string): void => {
        if (gap > EPS && gap < GRID - EPS)
            report("B7b", next.el, `sits ${round2(gap)}px ${axis} of its previous sibling: either flush at 0 or at least ${GRID}px`);
    };

    for (const record of records) {
        const { el, style } = record;
        const isColumn = style.display === "block" || style.display === "flow-root" ||
            ((style.display === "flex" || style.display === "inline-flex") && style.flexDirection.startsWith("column"));
        if (isColumn) {
            const kids = childrenOf(el).filter(isBlockLevel)
                    .sort((a, b) => a.rect.top - b.rect.top);
            for (let i = 1; i < kids.length; i++)
                reportGap(kids[i - 1], kids[i], kids[i].rect.top - kids[i - 1].rect.bottom, "below");
            continue;
        }
        if ((style.display === "flex" || style.display === "inline-flex") && style.flexDirection.startsWith("row")) {
            for (const line of intoLines(childrenOf(el))) {
                for (let i = 1; i < line.length; i++)
                    reportGap(line[i - 1], line[i], line[i].rect.left - line[i - 1].rect.right, "right");
            }
        }
    }

    // --- B7c: WCAG 2.5.8, a 24x24 hit target ------------------------------
    const INTERACTIVE = [
        "button",
        "a[href]",
        "input:not([type=hidden])",
        "select",
        "textarea",
        "summary",
        "[role=button]",
        "[role=checkbox]",
        "[role=switch]",
        "[role=radio]",
        "[role=tab]",
        "[role=menuitem]",
    ].join(", ");

    interface Target {
        el: Element;
        left: number;
        top: number;
        right: number;
        bottom: number;
        labelled: boolean;
    }

    const targets: Target[] = [];
    for (const { el, style, rect } of records) {
        if (!el.matches(INTERACTIVE))
            continue;
        if (el.matches("[disabled], [aria-disabled=true]"))
            continue;
        // WCAG's own inline exception: a link inside a sentence is sized by
        // the text around it and enlarging it would break the paragraph.
        if (style.display === "inline" && el.matches("a[href], [role=link]"))
            continue;

        // A checkbox is 13px square by design; what the user actually presses
        // is the control plus its label, so that is what gets measured. The
        // union only exists when a label is really associated -- an
        // `aria-label` string names the control for a screen reader and gives
        // a pointer nothing to hit.
        const id = el.getAttribute("id");
        const label = el.closest("label") ??
            (id ? document.querySelector(`label[for="${CSS.escape(id)}"]`) : null);
        const box = label ? label.getBoundingClientRect() : rect;
        targets.push({
            el,
            left: Math.min(rect.left, box.left),
            top: Math.min(rect.top, box.top),
            right: Math.max(rect.right, box.right),
            bottom: Math.max(rect.bottom, box.bottom),
            labelled: label !== null,
        });
    }

    /* The criterion is a size floor with a spacing escape, and both halves are
     * needed. A 13px checkbox alone in a table row conforms: nothing else is
     * near enough to mis-tap. The same checkbox with a second control 8px away
     * does not. Checking only the size floor would reject every row-select
     * column and every icon-button toolbar on the web, and a check that fires
     * on conformant markup is one somebody turns off.
     *
     * The test is a 24px circle on the undersized target's centre: it has to
     * clear every other target's box, or, when that neighbour is undersized
     * too, that neighbour's own circle. */
    for (const target of targets) {
        const width = target.right - target.left;
        const height = target.bottom - target.top;
        if (width >= MIN_TARGET - EPS && height >= MIN_TARGET - EPS)
            continue;

        const x = (target.left + target.right) / 2;
        const y = (target.top + target.bottom) / 2;
        const radius = MIN_TARGET / 2;
        const crowding = targets.find(other => {
            if (other === target || other.el.contains(target.el) || target.el.contains(other.el))
                return false;
            const otherX = (other.left + other.right) / 2;
            const otherY = (other.top + other.bottom) / 2;
            if (other.right - other.left < MIN_TARGET - EPS || other.bottom - other.top < MIN_TARGET - EPS)
                return Math.hypot(otherX - x, otherY - y) < MIN_TARGET - EPS;
            const dx = Math.max(other.left - x, 0, x - other.right);
            const dy = Math.max(other.top - y, 0, y - other.bottom);
            return Math.hypot(dx, dy) < radius - EPS;
        });
        if (crowding)
            report("B7c", target.el, `hit target is ${round2(width)}x${round2(height)}${target.labelled ? " across its label" : ""}, under the ${MIN_TARGET}x${MIN_TARGET} minimum, and ${describe(crowding.el)} sits inside the ${MIN_TARGET}px spacing that would otherwise excuse it`);
    }

    return {
        violations,
        usedExclusions: [...usedExclusions].sort((a, b) => a - b),
        stats: {
            elements: records.length,
            suppressed,
        },
    };
};
