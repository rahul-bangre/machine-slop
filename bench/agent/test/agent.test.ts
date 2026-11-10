import assert from "node:assert/strict";

import type Anthropic from "@anthropic-ai/sdk";

import { toolOutput } from "../src/agent.ts";
import { agentdProvider } from "../src/agentd_sandbox.ts";
import { type AttemptRecord, runAttempt } from "../src/attempt.ts";
import { type Model, model_id } from "../src/model.ts";
import type { SandboxProvider } from "../src/sandbox.ts";
import { failureCounts } from "../src/summary.ts";
import { loadTasks, type Task } from "../src/tasks.ts";

const sandbox = agentdProvider({ binary: "./target/debug/agentd" });
const limits = { max_turns: 10, command_timeout_ms: 20_000 };
const tasks = await loadTasks();

function reply(
    content: Anthropic.ContentBlock[],
    stop_reason: Anthropic.Message["stop_reason"],
): Anthropic.Message {
    return {
        id: "msg_scripted",
        type: "message",
        role: "assistant",
        model: model_id,
        content,
        stop_reason,
        stop_sequence: null,
        usage: { input_tokens: 100, output_tokens: 10 },
    } as Anthropic.Message;
}

function bash(id: string, command: string): Anthropic.ContentBlock {
    return {
        type: "tool_use",
        id,
        name: "bash",
        input: { command },
    } as Anthropic.ContentBlock;
}

const done = reply(
    [{ type: "text", text: "Done." } as Anthropic.ContentBlock],
    "end_turn",
);

function scriptedModel(replies: Anthropic.Message[]): Model {
    const queue = [...replies];
    return {
        describe: () => ({ model: "scripted" }),
        respond: () => Promise.resolve(queue.shift() ?? done),
    };
}

function writeSolution(task: Task): Anthropic.Message {
    return reply(
        Object.entries(task.solution).map(([path, content], index) =>
            bash(
                `call_${index}`,
                `cat > ${path} <<'BENCH_EOF'\n${content}\nBENCH_EOF`,
            )
        ),
        "tool_use",
    );
}

async function attempt(
    task: Task,
    model: Model,
    provider: SandboxProvider = sandbox,
): Promise<AttemptRecord> {
    const transcript_path = await Deno.makeTempFile({ suffix: ".json" });
    try {
        return await runAttempt({
            provider,
            task,
            round: 1,
            position_in_round: 0,
            model,
            limits,
            transcript_path,
        });
    } finally {
        await Deno.remove(transcript_path);
    }
}

for (const task of tasks) {
    Deno.test(`${task.id}: the checker rejects the starting files and accepts the reference solution`, async () => {
        const untouched = await attempt(task, scriptedModel([]));
        assert.equal(untouched.status, "failed");
        assert.equal(untouched.failure?.kind, "checker");

        const solved = await attempt(
            task,
            scriptedModel([writeSolution(task)]),
        );
        assert.equal(solved.failure, null);
        assert.equal(solved.status, "passed");
        assert.equal(
            solved.agent?.commands.length,
            Object.keys(task.solution).length,
        );
        assert.ok(
            solved.agent.commands.every((command) => command.exit_code === 0),
        );
    });
}

Deno.test("a model that never stops calling tools fails on the turn limit, and a hung command is killed", async () => {
    const task = tasks[0]!;
    const looping: Model = {
        describe: () => ({}),
        respond: () =>
            Promise.resolve(reply([bash("call", "true")], "tool_use")),
    };
    const exhausted = await attempt(task, looping);
    assert.equal(exhausted.failure?.kind, "turn_limit");
    assert.equal(exhausted.agent?.turns, limits.max_turns);

    const hung = await runAttempt({
        provider: sandbox,
        task,
        round: 1,
        position_in_round: 0,
        model: scriptedModel([reply([bash("call", "sleep 30")], "tool_use")]),
        limits: { max_turns: 3, command_timeout_ms: 300 },
        transcript_path: await Deno.makeTempFile(),
    });
    assert.equal(hung.agent?.commands[0]?.timed_out, true);
    assert.ok(hung.agent.commands[0].ms < 5_000);
});

Deno.test("a refusal counts against the model and a sandbox that cannot start counts against the sandbox", async () => {
    const task = tasks[0]!;
    const refused = await attempt(task, scriptedModel([reply([], "refusal")]));
    assert.equal(refused.failure?.kind, "model");

    const broken = await attempt(
        task,
        scriptedModel([]),
        agentdProvider({ binary: "./target/debug/no-such-agentd" }),
    );
    assert.equal(broken.failure?.kind, "sandbox");
    assert.equal(broken.agent, null);

    assert.deepEqual(failureCounts([refused, broken, refused]), {
        model: 2,
        sandbox: 1,
    });
});

Deno.test("long command output keeps its head, tail and exit status and says how much was dropped", () => {
    const output = toolOutput({
        exit_code: 3,
        stdout: "a".repeat(15_000) + "b".repeat(15_000),
        stderr: "",
        timed_out: false,
    });
    assert.ok(output.startsWith("aaaa"));
    assert.ok(output.includes("[10000 characters of output omitted]"));
    assert.ok(output.endsWith("bbbb\nexit code 3"));
});
