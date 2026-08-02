#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import process from "node:process";

import esbuild from "esbuild";

import { buildNotices } from "./build-notices.js";

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
