import { parseArgs } from "node:util";

import { writeArtifact } from "./artifacts.ts";
import { daytonaProvider } from "./daytona.ts";
import { e2bProvider } from "./e2b.ts";
import { browserEnvironment } from "./environment.ts";
import {
    type ImageRecord,
    inputSha256,
    type Provider,
    ProviderNotReady,
} from "./provider.ts";
import { guestFiles, smoke, StaleImage } from "./smoke.ts";

const usage = "usage: env <build|smoke> <e2b|daytona> [--out <dir>]";
const { positionals, values: flags } = parseArgs({
    allowPositionals: true,
    options: { out: { type: "string" } },
});

function fail(message: string): never {
    console.error(message);
    Deno.exit(2);
}

function provider(name: string | undefined): Provider {
    try {
        if (name === "e2b") return e2bProvider();
        if (name === "daytona") return daytonaProvider();
    } catch (error) {
        if (error instanceof ProviderNotReady) fail(error.message);
        throw error;
    }
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

if (command === "build") {
    const target = provider(name);
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
    const target = provider(name);
    const record = await readRecord(target.name);
    const run_id = crypto.randomUUID();
    const report = await smoke(
        target,
        environment,
        record,
        await guestFiles(),
        run_id,
    ).catch((error) => {
        if (error instanceof StaleImage) fail(error.message);
        throw error;
    });
    const directory = flags.out ??
        `bench-results/env-${new Date().toISOString().replaceAll(":", "-")}`;
    await Deno.mkdir(directory, { recursive: true });
    await writeArtifact(`${directory}/${target.name}.json`, report);
    for (const check of report.checks) {
        console.log(
            `${check.ok ? "ok  " : "FAIL"} ${check.name}: ${check.observed}`,
        );
    }
    if (report.failure) console.log(`FAIL ${report.failure}`);
    console.log(`report written to ${directory}/${target.name}.json`);
    if (!report.passed) Deno.exit(1);
} else {
    fail(usage);
}
