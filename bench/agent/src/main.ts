import { parseArgs } from "node:util";

import { armOrder } from "../../common/schedule.ts";
import { wholeNumber } from "../../common/stats.ts";
import type { Limits } from "./agent.ts";
import {
    agentdProvider,
    machinedApiKey,
    machinedProvider,
} from "./agentd-sandbox.ts";
import { type AttemptRecord, runAttempt } from "./attempt.ts";
import { daytonaProvider } from "./daytona-sandbox.ts";
import { e2bProvider } from "./e2b-sandbox.ts";
import { claudeCliModel } from "./claude-cli-model.ts";
import { claudeModel, default_claude_model } from "./claude-model.ts";
import type { Model } from "./model.ts";
import { openaiModel } from "./openai-model.ts";
import { ProviderNotReady, type SandboxProvider } from "./sandbox.ts";
import { renderSummary } from "./summary.ts";
import { loadTasks } from "./tasks.ts";

const { values: flags } = parseArgs({
    options: {
        sandboxes: { type: "string", default: "agentd" },
        model: {
            type: "string",
            default: `claude-cli:${default_claude_model}`,
        },
        tasks: { type: "string" },
        rounds: { type: "string", default: "3" },
        "max-turns": { type: "string", default: "30" },
        "command-timeout-ms": { type: "string", default: "60000" },
        "agentd-url": { type: "string" },
        "agentd-binary": { type: "string", default: "./target/debug/agentd" },
        "machined-url": { type: "string", default: "http://127.0.0.1:4001" },
        "e2b-template": { type: "string", default: "base" },
        out: { type: "string" },
    },
});

function fail(message: string): never {
    console.error(message);
    Deno.exit(2);
}

function names(flag: string): string[] {
    return flag.split(",").map((name) => name.trim());
}

function provider(name: string): SandboxProvider {
    if (name === "agentd") {
        return agentdProvider(
            flags["agentd-url"]
                ? { url: flags["agentd-url"] }
                : { binary: flags["agentd-binary"] },
        );
    }
    if (name === "machined") {
        return machinedProvider(flags["machined-url"], machinedApiKey());
    }
    if (name === "daytona") return daytonaProvider();
    if (name === "e2b") return e2bProvider(flags["e2b-template"]);
    fail(`unknown sandbox "${name}"; choose agentd, machined, e2b or daytona`);
}

function pinnedModel(spec: string): Model {
    const separator = spec.indexOf(":");
    const vendor = spec.slice(0, separator);
    const model_id = spec.slice(separator + 1);
    if (separator < 1 || model_id === "") {
        fail("--model takes <provider>:<model id>, such as openai:<model id>");
    }
    if (vendor === "claude-cli") return claudeCliModel(model_id);
    if (vendor === "anthropic") return claudeModel(model_id);
    if (vendor === "openai") {
        if (!Deno.env.get("OPENAI_API_KEY")) {
            fail("an openai model needs OPENAI_API_KEY");
        }
        return openaiModel(model_id);
    }
    fail(
        `unknown model provider "${vendor}"; choose claude-cli, anthropic or openai`,
    );
}

function settingsFromFlags(): { rounds: number; limits: Limits } {
    try {
        return {
            rounds: wholeNumber("rounds", flags.rounds, 1),
            limits: {
                max_turns: wholeNumber("max-turns", flags["max-turns"], 1),
                command_timeout_ms: wholeNumber(
                    "command-timeout-ms",
                    flags["command-timeout-ms"],
                    1,
                ),
            },
        };
    } catch (error) {
        if (error instanceof RangeError) fail(error.message);
        throw error;
    }
}

const { rounds, limits } = settingsFromFlags();
const providers = names(flags.sandboxes).map(provider);
const all_tasks = await loadTasks();
const tasks = flags.tasks
    ? names(flags.tasks).map((id) =>
        all_tasks.find((task) => task.id === id) ?? fail(`unknown task "${id}"`)
    )
    : all_tasks;
const model = pinnedModel(flags.model);

for (const sandbox of providers) {
    try {
        await sandbox.prepare();
    } catch (error) {
        if (error instanceof ProviderNotReady) fail(error.message);
        throw error;
    }
}

async function reclaimSandboxes(): Promise<void> {
    await Promise.allSettled(providers.map((sandbox) => sandbox.reclaim()));
}

Deno.addSignalListener("SIGINT", () => {
    console.error("interrupted; releasing sandboxes");
    reclaimSandboxes().finally(() => Deno.exit(130));
});

const started_at = new Date();
const output_directory = flags.out ??
    `bench-results/agent-${started_at.toISOString().replaceAll(":", "-")}`;
await Deno.mkdir(`${output_directory}/transcripts`, { recursive: true });

await Deno.writeTextFile(
    `${output_directory}/run.json`,
    JSON.stringify(
        {
            started_at: started_at.toISOString(),
            host: {
                os: Deno.build.os,
                os_release: Deno.osRelease(),
                arch: Deno.build.arch,
            },
            rounds,
            limits,
            sandbox_order: "rotated by round + task index",
            model: model.describe(),
            sandboxes: providers.map((sandbox) => ({
                name: sandbox.name,
                ...sandbox.describe(),
            })),
            tasks: tasks.map((task) => ({
                id: task.id,
                files: Object.keys(task.files),
            })),
        },
        null,
        2,
    ),
);

const attempts: AttemptRecord[] = [];
const attempt_log = await Deno.open(`${output_directory}/attempts.jsonl`, {
    create: true,
    append: true,
});
const encoder = new TextEncoder();

try {
    for (let round = 1; round <= rounds; round++) {
        for (const [task_index, task] of tasks.entries()) {
            const order = armOrder(providers, round, task_index);
            for (const [position_in_round, sandbox] of order.entries()) {
                const record = await runAttempt({
                    provider: sandbox,
                    task,
                    round,
                    position_in_round,
                    model,
                    limits,
                    transcript_path:
                        `${output_directory}/transcripts/${sandbox.name}-${task.id}-r${round}.json`,
                });
                attempts.push(record);
                await attempt_log.write(
                    encoder.encode(JSON.stringify(record) + "\n"),
                );
                console.error(
                    `round ${round} ${task.id} ${sandbox.name}: ${record.status}` +
                        (record.failure
                            ? ` (${record.failure.kind}: ${record.failure.message})`
                            : ""),
                );
            }
        }
    }
} finally {
    attempt_log.close();
    await reclaimSandboxes();
}

const summary = `# Agent bench ${started_at.toISOString()}\n\n` +
    renderSummary(
        attempts,
        providers.map((sandbox) => sandbox.name),
        tasks.map((task) => task.id),
    );
await Deno.writeTextFile(`${output_directory}/summary.md`, summary);
console.log(summary);
console.error(`results written to ${output_directory}`);
