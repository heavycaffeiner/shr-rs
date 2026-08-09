/*
 * SPDX-License-Identifier: LGPL-2.1-or-later
 *
 * Serves `dist/` at `/shr-rs/`, which is the path Cockpit itself mounts a
 * package at. The path matters: index.html reaches out of the package with
 * `../base1/cockpit.js` and `../../static/branding.css`, so the document has
 * to sit two segments deep for those to resolve to the same URLs a real
 * session requests. Playwright intercepts them from there.
 *
 * Deliberately dumb. It knows nothing about locales or fixtures -- index.html
 * asks for `po.js` and `cockpit.js` with no query string, so a server could
 * only tell the variants apart by sniffing headers. That branching lives in
 * `routes.ts` instead, where the test already knows which variant it wants.
 */

import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import { fileURLToPath } from "node:url";

const DIST = path.resolve(fileURLToPath(new URL("../../../dist", import.meta.url)));
const MOUNT = "/shr-rs/";

const TYPES: Record<string, string> = {
    ".css": "text/css; charset=utf-8",
    ".gif": "image/gif",
    ".html": "text/html; charset=utf-8",
    ".jpg": "image/jpeg",
    ".js": "text/javascript; charset=utf-8",
    ".json": "application/json; charset=utf-8",
    ".map": "application/json; charset=utf-8",
    ".png": "image/png",
    ".svg": "image/svg+xml",
    ".ttf": "font/ttf",
    ".txt": "text/plain; charset=utf-8",
    ".woff": "font/woff",
    ".woff2": "font/woff2",
};

/* The URL comes off a socket, so it is untrusted even here: resolve it and
 * refuse anything that climbs out of dist/. */
const resolveWithin = (root: string) => (urlPath: string): string | null => {
    const decoded = decodeURIComponent(urlPath);
    const resolved = path.resolve(root, `.${decoded.startsWith("/") ? decoded : `/${decoded}`}`);
    return resolved === root || resolved.startsWith(root + path.sep) ? resolved : null;
};

const resolveInDist = resolveWithin(DIST);

export interface HarnessServer {
    origin: string;
    /** The document URL: `dist/index.html` at the depth Cockpit mounts it. */
    pageUrl: string;
    close: () => Promise<void>;
}

export const startHarnessServer = async (): Promise<HarnessServer> => {
    if (!fs.existsSync(path.join(DIST, "index.html")))
        throw new Error(`${DIST}/index.html is missing: run \`npm run build\` before the layout audit`);

    const server = http.createServer((request, response) => {
        const url = new URL(request.url ?? "/", "http://localhost");
        if (!url.pathname.startsWith(MOUNT)) {
            response.writeHead(404).end("not found");
            return;
        }
        const rest = url.pathname.slice(MOUNT.length) || "index.html";
        const file = resolveInDist(rest);
        if (file === null || !fs.existsSync(file) || !fs.statSync(file).isFile()) {
            response.writeHead(404).end("not found");
            return;
        }
        response.writeHead(200, {
            "content-type": TYPES[path.extname(file).toLowerCase()] ?? "application/octet-stream",
            // A stale bundle would make the audit report yesterday's layout.
            "cache-control": "no-store",
        });
        fs.createReadStream(file).pipe(response);
    });

    await new Promise<void>((resolve, reject) => {
        server.once("error", reject);
        server.listen(0, "127.0.0.1", () => resolve());
    });

    const address = server.address();
    if (address === null || typeof address === "string")
        throw new Error("the harness server did not bind a TCP port");
    const origin = `http://127.0.0.1:${address.port}`;
    return {
        origin,
        pageUrl: `${origin}${MOUNT}index.html`,
        close: () => new Promise<void>(resolve => server.close(() => resolve())),
    };
};
