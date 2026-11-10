import { tasks } from "../../browser/src/tasks.ts";
import type { RequestRecord } from "../../browser/src/fixtures/data.ts";
import { FixtureServer } from "../../browser/src/fixtures/server.ts";
import { type Workflow, workflows } from "../../browser/src/workflows/mod.ts";

type TuningTask = {
    id: string;
    path: string;
    goal: string;
    partition: "tuning";
    seeds: readonly number[];
    check(requests: RequestRecord[]): string | null;
};

export type BrowserTask = TuningTask | (Workflow & { partition: "held-out" });

export type TaskSite = {
    origin: string;
    prompt: string;
    outcome(): Promise<string | null>;
    stop(): Promise<void>;
};

function task(id: string, path: string, goal: string): TuningTask {
    const source = tasks.find((task) => task.id === id);
    if (!source || source.outcome.source !== "server-request-log") {
        throw new Error(`missing independent outcome checker: ${id}`);
    }
    return {
        id,
        path,
        goal,
        partition: "tuning",
        seeds: [0],
        check: source.outcome.failure,
    };
}

export const browser_tasks: BrowserTask[] = [
    task(
        "shop-checkout",
        "/shop/",
        "Buy one Cobalt Notebook for Ada Lovelace, ada@example.com. Accept the terms and place the order exactly once.",
    ),
    task(
        "dashboard-ticket",
        "/dashboard/",
        'Find the account with the highest revenue and file exactly one ticket for that account with the note "Renewal at risk".',
    ),
    ...workflows.map((workflow) => ({
        ...workflow,
        partition: "held-out" as const,
    })),
];

export function taskPrompt(task: BrowserTask, seed: number): string {
    return task.partition === "tuning" ? task.goal : task.open(seed).prompt;
}

export function openTask(task: BrowserTask, seed: number): TaskSite {
    if (task.partition === "tuning") {
        const fixture = new FixtureServer("http://127.0.0.1:0");
        return {
            origin: fixture.origin + task.path,
            prompt: task.goal,
            outcome: () => Promise.resolve(task.check(fixture.recorded())),
            stop: () => fixture.stop(),
        };
    }
    const { site, prompt } = task.open(seed);
    const server = Deno.serve(
        { hostname: "127.0.0.1", port: 0, onListen() {} },
        (request) => site.handle(request),
    );
    return {
        origin: `http://127.0.0.1:${server.addr.port}/`,
        prompt,
        outcome: () => site.outcome(),
        stop: () => server.shutdown(),
    };
}

export function browserTask(id: string): BrowserTask {
    const task = browser_tasks.find((task) => task.id === id);
    if (!task) throw new Error(`unknown browser task: ${id}`);
    return task;
}
