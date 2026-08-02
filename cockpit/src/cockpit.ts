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
}

declare global {
    interface Window {
        cockpit: CockpitApi;
    }
}

if (!window.cockpit)
    throw new Error("Cockpit JavaScript API를 불러오지 못했습니다.");

export default window.cockpit;
