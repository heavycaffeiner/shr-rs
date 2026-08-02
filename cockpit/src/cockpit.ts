export interface CockpitError {
    message?: string;
    problem?: string | null;
    exit_status?: number | null;
    exit_signal?: string | null;
}

export interface SpawnOptions {
    err?: "out" | "ignore" | "message";
    superuser?: "require" | "try";
}

export interface CockpitApi {
    spawn(args: string[], options?: SpawnOptions): Promise<string>;

    /* Localization. Everything below is optional on this interface because the
     * unit tests stub `window.cockpit` with `spawn` alone; `i18n.ts` carries
     * the fallbacks. On a real page `../base1/cockpit.js` defines all of it,
     * and `po-default.js`/`po.js` have already called `cockpit.locale()` with
     * this package's catalogues before our bundle runs -- see index.html for
     * that load order. */
    gettext?: ((message: string) => string) & ((context: string, message: string) => string);
    ngettext?: (message1: string, messageN: string, n: number) => string;
    format?: (format_string: string, ...args: unknown[]) => string;
    language?: string;
    language_direction?: string;
}

declare global {
    interface Window {
        cockpit: CockpitApi;
    }
}

if (!window.cockpit)
    throw new Error("Failed to load the Cockpit JavaScript API.");

export default window.cockpit;
