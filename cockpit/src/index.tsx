/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 */

import React from "react";
import { createRoot } from "react-dom/client";

import { Application } from "./app.js";
import { initDarkTheme } from "./darkTheme.js";
import { applyDocumentLanguage } from "./i18n.js";

// The plugin's entire stylesheet. There is no local CSS: base carries the
// tokens, resets and @font-face rules, addons carries the `pf-v6-u-*`
// utilities, and each PatternFly component barrel-imported under `src/`
// side-effect-imports its own CSS on top of these.
import "@patternfly/patternfly/patternfly-base.css";
import "@patternfly/patternfly/patternfly-addons.css";

// Must run before first paint: PatternFly 6 tokens key off the
// `.pf-v6-theme-dark` class, which nothing else sets on a plugin document.
initDarkTheme();

// The catalogues have already run by now (index.html loads them first), so
// this is the session's real language rather than index.html's static
// `lang="en"`.
applyDocumentLanguage();

document.addEventListener("DOMContentLoaded", () => {
    createRoot(document.getElementById("app")!).render(<Application />);
});
