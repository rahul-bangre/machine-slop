import { parseArgs } from "node:util";
import {
    agentdProvider,
    machinedProvider,
} from "../../agent/src/agentd-sandbox.ts";
import { claudeCliModel } from "../../agent/src/claude-cli-model.ts";
import { daytonaProvider } from "../../agent/src/daytona-sandbox.ts";
import { e2bProvider } from "../../agent/src/e2b-sandbox.ts";
import { default_claude_model } from "../../agent/src/claude-model.ts";
import type { SandboxProvider } from "../../agent/src/sandbox.ts";
import { armOrder } from "../../common/schedule.ts";
import { wholeNumber } from "../../common/stats.ts";
import {
    pinned_product,
    pinnedExecutable,
} from "../../browser/src/chromium.ts";
import { browser_tasks, browserTask } from "../runtime/tasks.ts";
import { type Attempt, runBrowserAttempt } from "./attempt.ts";
import { deno_version, payload, RemoteRuntime } from "./deploy.ts";
import { browser_system_prompt } from "./prompt.ts";
import e2b_image from "../../env/images/e2b.json" with { type: "json" };
import daytona_image from "../../env/images/daytona.json" with { type: "json" };

const { values: flags } = parseArgs({
    options: {
        sandboxes: { type: "string", default: "agentd" },
        engines: { type: "string", default: "playwright,native" },
        tasks: { type: "string" },
        rounds: { type: "string", default: "3" },
        model: { type: "string", default: default_claude_model },
        "max-turns": { type: "string", default: "20" },
        "agentd-binary": { type: "string", default: "./target/debug/agentd" },
        "agentd-url": { type: "string" },
        "machined-url": { type: "string", default: "http://127.0.0.1:4001" },
        "e2b-template": { type: "string", default: e2b_image.image.ref },
        "daytona-snapshot": {
            type: "string",
            default: daytona_image.image.ref,
        },
        "browser-executable": { type: "string" },
        deno: { type: "string", default: "deno" },
        out: { type: "string" },
    },
});

function provider(name: string): SandboxProvider {
    switch (name) {
        case "agentd":
            return agentdProvider(
                flags["agentd-url"]
                    ? { url: flags["agentd-url"] }
                    : { binary: flags["agentd-binary"] },
            );
        case "machined":
            return machinedProvider(flags["machined-url"]);
        case "e2b":
            return e2bProvider(flags["e2b-template"], {
                cpu_count: 2,
                memory_mb: 4096,
            });
        case "daytona":
            return daytonaProvider(undefined, flags["daytona-snapshot"]);
        default:
            throw new Error(`unknown sandbox: ${name}`);
    }
}

const providers = flags.sandboxes.split(",").map(provider);
const engines = flags.engines.split(",").map((engine): Attempt["engine"] => {
    if (engine === "native" || engine === "playwright") return engine;
    throw new Error(`unknown engine: ${engine}`);
});
const tasks = flags.tasks
    ? flags.tasks.split(",").map(browserTask)
    : browser_tasks.filter((task) => task.partition === "tuning");
const rounds = wholeNumber("rounds", flags.rounds, 1);
const limits = {
    max_turns: wholeNumber("max-turns", flags["max-turns"], 1),
    command_timeout_ms: 40_000,
    max_tool_output_chars: 128_000,
};
if (
    providers.some((provider) => provider.name !== "agentd") &&
    !flags["browser-executable"]
) {
    throw new Error(
        "remote sandboxes require --browser-executable pointing to the pinned guest Chromium; install Deno and Chromium in the image first",
    );
}
const browser = {
    name: "stock-chromium",
    executable: flags["browser-executable"] ?? pinnedExecutable(),
    product: pinned_product,
    args: ["--no-sandbox"],
};
const cancellation = new AbortController();
const cli_version = await new Deno.Command("claude", {
    args: ["--version"],
    stdout: "piped",
    stderr: "piped",
}).output();
if (!cli_version.success) throw new Error("Claude CLI is not available");
const model = claudeCliModel(
    flags.model,
    browser_system_prompt,
    cancellation.signal,
);
const bundle = await payload();
const started = new Date();
const out = flags.out ??
    `bench-results/browser-agent-${started.toISOString().replaceAll(":", "-")}`;
await Deno.mkdir(`${out}/transcripts`, { recursive: true });
await Deno.writeTextFile(
    `${out}/run.json`,
    JSON.stringify(
        {
            started_at: started.toISOString(),
            model: model.describe(),
            cli_version: new TextDecoder().decode(cli_version.stdout).trim(),
            browser,
            deno_version,
            payload_sha256: bundle.sha256,
            limits,
            rounds,
            engines,
            sandboxes: providers.map((provider) => ({
                name: provider.name,
                ...provider.describe(),
            })),
            image_records: {
                e2b: flags["e2b-template"] === e2b_image.image.ref
                    ? e2b_image
                    : null,
                daytona: flags["daytona-snapshot"] === daytona_image.image.ref
                    ? daytona_image
                    : null,
            },
            network:
                "enabled for dependency setup and task; all task sites are served on guest loopback",
            tasks: tasks.map(({ id, seeds, partition }) => ({
                id,
                seeds,
                partition,
            })),
            order:
                "rotate providers and engines independently by round + task index",
            scope:
                "root-document semantic text/control tasks; scripted fixture server with subscription-backed agent decisions; no provider superiority or billed cost claim",
        },
        null,
        2,
    ),
    { createNew: true },
);

const reclaim = async () => {
    await RemoteRuntime.reclaim().catch((error) =>
        console.error("runtime cleanup failed:", error)
    );
    const results = await Promise.allSettled(
        providers.map((provider) => provider.reclaim()),
    );
    for (const result of results) {
        if (result.status === "rejected") {
            console.error("cleanup failed:", result.reason);
        }
    }
};
const interrupted = () => {
    cancellation.abort();
    reclaim().finally(() => Deno.exit(130));
};
Deno.addSignalListener("SIGINT", interrupted);
Deno.addSignalListener("SIGTERM", interrupted);
const attempts: Attempt[] = [];
const log = await Deno.open(`${out}/attempts.jsonl`, {
    createNew: true,
    write: true,
});
try {
    for (const provider of providers) await provider.prepare();
    for (
        const round of Array.from({ length: rounds }, (_, index) => index + 1)
    ) {
        for (const [task_index, task] of tasks.entries()) {
            for (const provider of armOrder(providers, round, task_index)) {
                for (
                    const [position, engine] of armOrder(
                        engines,
                        round,
                        task_index,
                    ).entries()
                ) {
                    const record = await runBrowserAttempt({
                        provider,
                        engine,
                        task,
                        round,
                        seed: task.seeds[(round - 1) % task.seeds.length]!,
                        position,
                        model,
                        limits,
                        browser,
                        deno: flags.deno,
                        files: bundle.files,
                        artifact:
                            `${out}/transcripts/${provider.name}-${engine}-${task.id}-r${round}.json`,
                    });
                    attempts.push(record);
                    await log.write(
                        new TextEncoder().encode(JSON.stringify(record) + "\n"),
                    );
                    console.error(
                        `${provider.name} ${engine} ${task.id} r${round}: ${record.status}${
                            record.failure ? ` (${record.failure})` : ""
                        }`,
                    );
                }
            }
        }
    }
} finally {
    log.close();
    await reclaim();
    Deno.removeSignalListener("SIGINT", interrupted);
    Deno.removeSignalListener("SIGTERM", interrupted);
}
console.log(
    `${
        attempts.filter((record) => record.status === "passed").length
    }/${attempts.length} passed; raw attempts and transcripts: ${out}`,
);
