import { parseArgs } from "node:util";

import { writeArtifact } from "./artifacts.ts";
import { daytonaProvider } from "./daytona.ts";
import { e2bProvider } from "./e2b.ts";
import { browserEnvironment, type Environment } from "./environment.ts";
import { machinedApiKey } from "../../agent/src/agentd-sandbox.ts";
import { machinedSandboxes } from "./machined.ts";
import {
    type ImageBuilder,
    type ImageRecord,
    inputSha256,
    ProviderNotReady,
    type Sandboxes,
} from "./provider.ts";
import { guestFiles, recordedSandboxes, smoke, StaleImage } from "./smoke.ts";

const usage =
    "usage: env build <e2b|daytona> | env smoke <e2b|daytona|machined> [--out <dir>] [--machined-url <url>]";
const { positionals, values: flags } = parseArgs({
    allowPositionals: true,
    options: {
        out: { type: "string" },
        "machined-url": { type: "string", default: "http://127.0.0.1:4001" },
    },
});

function fail(message: string): never {
    console.error(message);
    Deno.exit(2);
}

async function ready<T>(start: () => T | Promise<T>): Promise<T> {
    try {
        return await start();
    } catch (error) {
        if (error instanceof ProviderNotReady) fail(error.message);
        throw error;
    }
}

function builder(name: string | undefined): Promise<ImageBuilder> {
    if (name === "e2b") return ready(e2bProvider);
    if (name === "daytona") return ready(daytonaProvider);
    fail(usage);
}

function recordPath(name: string): string {
    return new URL(`../images/${name}.json`, import.meta.url).pathname;
}

async function readRecord(name: string): Promise<ImageRecord> {
    try {
        return JSON.parse(await Deno.readTextFile(recordPath(name)));
    } catch (error) {
        if (error instanceof Deno.errors.NotFound) {
            fail(`no ${name} image is recorded; run env build ${name} first`);
        }
        throw error;
    }
}

const [command, name] = positionals;
if (positionals.length !== 2) fail(usage);
const environment = await browserEnvironment();

async function smokeTarget(
    name: string | undefined,
    environment: Environment,
): Promise<{ sandboxes: Sandboxes; image: ImageRecord | null }> {
    if (name === "machined") {
        return {
            sandboxes: await ready(() =>
                machinedSandboxes(
                    flags["machined-url"],
                    machinedApiKey(),
                    environment.resources,
                )
            ),
            image: null,
        };
    }
    const target = await builder(name);
    const image = await readRecord(target.name);
    try {
        return {
            sandboxes: await recordedSandboxes(target, environment, image),
            image,
        };
    } catch (error) {
        if (error instanceof StaleImage) fail(error.message);
        throw error;
    }
}

if (command === "build") {
    const target = await builder(name);
    await Deno.mkdir(new URL("../images/", import.meta.url), {
        recursive: true,
    });
    const input_sha256 = await inputSha256(target, environment);
    const image = await target.build(environment, input_sha256);
    const record: ImageRecord = {
        provider: target.name,
        sdk: target.sdk,
        input_sha256,
        resources: environment.resources,
        image,
        built_at: new Date().toISOString(),
    };
    await writeArtifact(recordPath(target.name), record);
    console.log(`${target.name}: built ${image.ref} (${image.id})`);
} else if (command === "smoke") {
    const { sandboxes, image } = await smokeTarget(name, environment);
    const report = await smoke(
        sandboxes,
        image,
        environment,
        await guestFiles(),
        crypto.randomUUID(),
    );
    const directory = flags.out ??
        `bench-results/env-${new Date().toISOString().replaceAll(":", "-")}`;
    await Deno.mkdir(directory, { recursive: true });
    await writeArtifact(`${directory}/${sandboxes.name}.json`, report);
    for (const check of report.checks) {
        console.log(
            `${check.ok ? "ok  " : "FAIL"} ${check.name}: ${check.observed}`,
        );
    }
    if (report.failure) console.log(`FAIL ${report.failure}`);
    console.log(`report written to ${directory}/${sandboxes.name}.json`);
    if (!report.passed) Deno.exit(1);
} else {
    fail(usage);
}
