/*
 * Copyright (C) 2022 Red Hat, Inc.
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Ported from Cockpit's own `pkg/lib/cockpit-dark-theme.ts` (same licence),
 * because a plugin page is a separate document from the shell and nothing
 * sets the theme class on it for us.
 *
 * This is load-bearing, not cosmetic. PatternFly 6 tokens switch on the
 * `.pf-v6-theme-dark` CLASS, not on `prefers-color-scheme`. Before the
 * PatternFly migration this plugin styled dark mode with a
 * `@media (prefers-color-scheme: dark)` block, which appeared to track the
 * Cockpit theme switcher only because Chrome propagates an embedder's used
 * color-scheme into same-origin iframes. That coincidence does not carry
 * the PatternFly tokens: without this module, choosing "밝게"/"어둡게" in
 * Cockpit's session menu would leave every PatternFly surface unchanged.
 *
 * Cockpit stores the preference in `localStorage["shell:style"]` as one of
 * `auto` | `light` | `dark`, and shares that origin with us. Three signals
 * can change it, and all three are needed:
 *   - `storage`       -- another document (the shell) wrote the key
 *   - `cockpit-style` -- the shell's own switcher, which does NOT raise a
 *                        `storage` event for the page that performed it
 *   - `matchMedia`    -- the OS preference moved while style is `auto`
 */

const THEME_CLASS = "pf-v6-theme-dark";

const applyDarkThemeClass = (dark: boolean): void => {
    document.documentElement.classList.toggle(THEME_CLASS, dark);
};

/** Resolves the effective mode. An explicit `light`/`dark` wins over the OS
 * preference; only `auto` defers to `prefers-color-scheme`. */
export const resolveDarkMode = (
    style: string | null,
    prefersDark: boolean,
): boolean => {
    const effective = style || "auto";
    if (effective === "dark")
        return true;
    if (effective === "light")
        return false;
    return prefersDark;
};

const setDarkMode = (style?: string): void => {
    const prefersDark = window.matchMedia?.("(prefers-color-scheme: dark)").matches ?? false;
    applyDarkThemeClass(resolveDarkMode(style ?? localStorage.getItem("shell:style"), prefersDark));
};

/** Wires the three signals and applies the current theme immediately. */
export const initDarkTheme = (): void => {
    window.addEventListener("storage", event => {
        if (event.key === "shell:style")
            setDarkMode();
    });

    // The shell's switcher does not raise `storage` in the document that
    // performed the write, so Cockpit re-broadcasts it as a CustomEvent.
    window.addEventListener("cockpit-style", event => {
        if (event instanceof CustomEvent)
            setDarkMode(event.detail?.style);
    });

    window.matchMedia?.("(prefers-color-scheme: dark)")
            .addEventListener("change", () => setDarkMode());

    setDarkMode();
};
