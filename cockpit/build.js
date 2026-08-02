#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import process from "node:process";

import esbuild from "esbuild";

import { buildNotices } from "./build-notices.js";
import { cockpitPoEsbuildPlugin } from "./pkg/lib/cockpit-po-plugin.js";

const watch = process.argv.includes("--watch") || process.argv.includes("-w");
const production = process.env.NODE_ENV === "production";
const outdir = path.resolve("dist");
const runtimeModulesFile = path.resolve("runtime-npm-modules.txt");

fs.rmSync(outdir, { recursive: true, force: true });
fs.mkdirSync(outdir, { recursive: true });

const copyAssetsPlugin = {
    name: "copy-static-assets",
    setup(build) {
        build.onEnd(result => {
            if (result.errors.length > 0)
                return;
            fs.copyFileSync("src/manifest.json", path.join(outdir, "manifest.json"));
            fs.copyFileSync("src/index.html", path.join(outdir, "index.html"));
        });
    },
};

// The msgids in `src/` are dotted keys ("panels.drives.col.node"), not
// English sentences, so English is a translation like any other and lives in
// `po/en.po`. Cockpit does not expect that, and measuring it on a real
// Cockpit 356 guest is what settled how to handle it:
//
//   - `GET po.js` with `CockpitLang=ko` serves `po.ko.js`.       (200, 39 kB)
//   - `GET po.js` with `CockpitLang=en`, `ja`, or none serves an
//     EMPTY body -- not a file named `po.js`, even when one exists,
//     because cockpit-ws treats English as "the msgids themselves"
//     and an untranslated language as "nothing to send".          (200, 0 B)
//   - `GET po.en.js` directly is a 404: the per-language names are
//     reachable only through that negotiation, never by hand.
//
// So the English catalogue cannot be delivered under any `po.*` name. It goes
// out as `po-default.js`, which carries no language segment for cockpit-ws to
// negotiate on and is therefore served verbatim to every session, and
// index.html loads it *before* `po.js`. `cockpit.locale()` merges
// (`Object.assign`), so a Korean session applies English first and Korean on
// top -- which also means a future partial translation falls back to English
// per-string instead of showing raw keys.
//
// Runs as its own plugin rather than after `rebuild()` so `--watch` gets it
// too, and registered after `cockpitPoEsbuildPlugin` because esbuild invokes
// `onEnd` hooks in registration order -- `po.en.js` has to exist first.
const defaultCatalogPlugin = {
    name: "default-po-catalog",
    setup(build) {
        build.onEnd(result => {
            if (result.errors.length > 0)
                return;
            fs.copyFileSync(path.join(outdir, "po.en.js"), path.join(outdir, "po-default.js"));
        });
    },
};

const context = await esbuild.context({
    bundle: true,
    entryPoints: ["src/index.tsx"],
    legalComments: "external",
    // PatternFly's base stylesheet carries @font-face rules for the Red Hat
    // variable fonts plus its pficon/FA glyph fonts. We emit them alongside
    // the bundle rather than pointing at Cockpit's `static/fonts` copies:
    // the development guide is explicit that a package must not assume it
    // can link into another package's files, and self-hosting guarantees the
    // exact faces PatternFly's tokens were designed against.
    loader: {
        ".woff": "file",
        ".woff2": "file",
        ".ttf": "file",
        ".eot": "file",
        ".svg": "file",
        ".png": "file",
        ".jpg": "file",
        ".gif": "file",
    },
    assetNames: "assets/[name]-[hash]",
    metafile: true,
    minify: production,
    outdir,
    // No Sass plugin: this package has no stylesheet of its own. All styling
    // comes from PatternFly -- `patternfly-base.css`/`patternfly-addons.css`
    // imported by `index.tsx`, plus the per-component CSS each PatternFly
    // barrel import pulls in as a side effect.
    plugins: [
        copyAssetsPlugin,
        // Compiles every po/*.po into `po.<lang>.js` (pages) and
        // `po.manifest.<lang>.js` (the shell's menu entry). index.html asks
        // for the extensionless `po.js`, and cockpit-ws rewrites that request
        // to the catalogue matching the session language.
        cockpitPoEsbuildPlugin(),
        defaultCatalogPlugin,
    ],
    sourcemap: production ? false : "linked",
    target: ["es2020"],
});

const writeRuntimeModules = metafile => {
    const lock = JSON.parse(fs.readFileSync("package-lock.json", "utf8"));
    const packages = new Set();
    for (const input of Object.keys(metafile.inputs)) {
        const normalized = input.replaceAll("\\", "/");
        const marker = "node_modules/";
        const markerIndex = normalized.lastIndexOf(marker);
        if (markerIndex < 0)
            continue;
        const segments = normalized.slice(markerIndex + marker.length).split("/");
        packages.add(segments[0].startsWith("@") ? `${segments[0]}/${segments[1]}` : segments[0]);
    }
    const lines = [...packages]
            .sort()
            .map(name => {
                const version = lock.packages?.[`node_modules/${name}`]?.version;
                if (!version)
                    throw new Error(`package-lock.json has no version for bundled module ${name}`);
                return `${name} ${version}`;
            });
    fs.writeFileSync(runtimeModulesFile, `${lines.join("\n")}\n`);
};

try {
    if (watch) {
        await context.watch();
        console.log("Watching cockpit/src for changes...");
    } else {
        const result = await context.rebuild();
        fs.writeFileSync("metafile.json", JSON.stringify(result.metafile, null, 2));
        writeRuntimeModules(result.metafile);
        buildNotices(outdir);
        await context.dispose();
        console.log(`Built SHR-RS Cockpit plugin in ${path.relative(process.cwd(), outdir)}`);
    }
} catch (error) {
    await context.dispose();
    console.error(error);
    process.exitCode = 1;
}
