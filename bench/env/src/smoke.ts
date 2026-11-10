import type { Observation } from "../guest/observe.ts";
import { type Check, evaluate, type GuestFacts } from "./checks.ts";
import type { Environment } from "./environment.ts";
import {
    type Guest,
    type ImageRecord,
    inputSha256,
    type Owner,
    type Provider,
} from "./provider.ts";

export const guest_directory = "/tmp/env-smoke";
const observe_timeout_ms = 180_000;

export type GuestFiles = { observer: string; fixture: string };

// `failed` holds every sandbox whose destruction could not be confirmed.
export type Release = { destroyed: string[]; failed: string[] };

export type SmokeReport = {
    provider: string;
    run_id: string;
    image: ImageRecord;
    passed: boolean;
    failure: string | null;
    guest: (GuestFacts & { id: string }) | null;
    checks: Check[];
    release: Release | { error: string };
    observation: Observation | null;
};

export class StaleImage extends Error {
    constructor(provider: string) {
        super(
            `the recorded ${provider} image was built from different inputs; rebuild it`,
        );
        this.name = "StaleImage";
    }
}

export async function guestFiles(): Promise<GuestFiles> {
    const read = (name: string) =>
        Deno.readTextFile(new URL(`../guest/${name}`, import.meta.url));
    return {
        observer: await read("observe.ts"),
        fixture: await read("fixture.html"),
    };
}

async function observeIn(
    guest: Guest,
    environment: Environment,
    files: GuestFiles,
): Promise<Observation> {
    const observer = `${guest_directory}/observe.ts`;
    const fixture = `${guest_directory}/fixture.html`;
    const made = await guest.exec(`mkdir -p ${guest_directory}`, 30_000);
    if (made.exit_code !== 0) {
        throw new Error(`cannot create ${guest_directory}: ${made.output}`);
    }
    await guest.write(observer, files.observer);
    await guest.write(fixture, files.fixture);
    const result = await guest.exec(
        `deno run --no-config --no-lock --allow-all ${observer} ${environment.chromium.executable} ${fixture}`,
        observe_timeout_ms,
    );
    const last_line = result.output.trim().split("\n").at(-1) ?? "";
    if (result.exit_code !== 0 || !last_line.startsWith("{")) {
        throw new Error(
            `observer exited ${result.exit_code}: ${
                result.output.slice(-2000)
            }`,
        );
    }
    return JSON.parse(last_line);
}

// Destroys every sandbox labelled with this run's owner, which includes one
// whose create response was lost, and the sandbox it started, which a
// provider's listing may not show yet. A listing can also still show a sandbox
// after it is gone, so each destruction is confirmed by its own ID instead of
// by listing again.
async function release(
    provider: Provider,
    owner: Owner,
    started: string | null,
): Promise<Release> {
    const ids = new Set(await provider.owned(owner));
    if (started) ids.add(started);
    const destroyed: string[] = [];
    const failed: string[] = [];
    for (const id of ids) {
        try {
            await provider.destroy(id);
            destroyed.push(id);
        } catch (error) {
            console.error(`cannot destroy ${id}: ${error}`);
            failed.push(id);
        }
    }
    return { destroyed, failed };
}

export async function smoke(
    provider: Provider,
    environment: Environment,
    record: ImageRecord,
    files: GuestFiles,
    run_id: string,
): Promise<SmokeReport> {
    if (record.input_sha256 !== await inputSha256(provider, environment)) {
        throw new StaleImage(provider.name);
    }
    const owner = { "env-smoke-run": run_id };
    let guest: SmokeReport["guest"] = null;
    let started_id: string | null = null;
    let observation: Observation | null = null;
    let failure: string | null = null;
    try {
        const started = await provider.start(record.image, owner);
        started_id = started.id;
        guest = {
            id: started.id,
            reported: started.reported,
        };
        observation = await observeIn(started, environment, files);
    } catch (error) {
        failure = error instanceof Error ? error.message : String(error);
    }
    const released = await release(provider, owner, started_id).catch((
        error,
    ) => ({
        error: error instanceof Error ? error.message : String(error),
    }));

    const checks = guest && observation
        ? evaluate(environment, guest, observation)
        : [];
    checks.push({
        name: "every sandbox this run created was destroyed",
        ok: "failed" in released && released.failed.length === 0,
        expected: "every one confirmed gone",
        observed: "failed" in released
            ? (released.failed.length
                ? `not confirmed: ${released.failed.join(", ")}`
                : `${released.destroyed.length} confirmed gone`)
            : released.error,
    });
    return {
        provider: provider.name,
        run_id,
        image: record,
        passed: failure === null && checks.every((check) => check.ok),
        failure,
        guest,
        checks,
        release: released,
        observation,
    };
}
