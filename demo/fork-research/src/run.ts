import { Host, type HostedMachine } from "../../../packages/sdk/src/mod.ts";
import { AgentSession } from "../../common/claude.ts";
import { type Browser, delay, type Page } from "../../common/browser.ts";
import { shell, tail } from "../../common/shell.ts";
import {
    connectChromium,
    readPage,
    startChromium,
} from "../../common/machine-browser.ts";

const home = Deno.env.get("HOME")!;
const machined_url = Deno.env.get("MACHINED_URL") ?? "http://127.0.0.1:4100";
const api_key = (await Deno.readTextFile(
    Deno.env.get("API_KEY_FILE") ?? `${home}/machine-slop/demo/api-key`,
)).trim();
const model = Deno.env.get("MODEL") ?? "claude-opus-5-5";
const repository = Deno.env.get("REPOSITORY") ?? "rahul-bangre/machine-slop";
const run_dir = `${home}/machine-slop/demo/runs/fork-research-${
    new Date().toISOString().replace(/[:.]/g, "-")
}`;
const viewport = { width: 1280, height: 800 };
const copies = 3;

const prompt =
    `Compare machine-slop with E2B. Skim our GitHub repo and E2B's docs, pick the 3 most important things E2B can do that we can't, research each one in depth, then open one GitHub issue on ${repository} with a section per gap, each linking to the E2B docs.`;

const allowed_hosts = [
    "github.com",
    "*.github.com",
    "*.githubusercontent.com",
    "*.githubassets.com",
    "e2b.dev",
    "*.e2b.dev",
];

const system_prompt = [
    "You are an agent with a Linux computer and a web browser.",
    "Every reply is JSON. `intent` is one plain-English sentence of at most 12 words saying what you will do next and why, for a person watching your screen.",
    '`action` is one of: "open" to load `url` in the browser (you get back the page text and its links); "shell" to run `command` with bash in /workspace; "fork" with `branches`; "done" with a one-sentence `summary`.',
    `"fork" copies your whole computer (browser, files) and this conversation ${copies} times. \`branches\` must list exactly ${copies} short topics; copy N researches topic N in parallel with the others.`,
    "Read with the browser. Use the shell only to write files and to create the issue.",
    "The internet is limited to github.com and e2b.dev and their subdomains.",
    `To create an issue, POST JSON to https://api.github.com/repos/${repository}/issues with curl, writing the body to a file first and sending it with --data @file. You have no GitHub token and need none: the network adds authentication for api.github.com.`,
    'Start the issue title with "Gaps vs E2B:". Keep the issue under 350 words, in Markdown.',
].join("\n");

type Step = {
    intent: string;
    action: "open" | "shell" | "fork" | "done";
    url?: string;
    command?: string;
    branches?: string[];
    summary?: string;
};

const step_schema = {
    type: "object",
    properties: {
        intent: { type: "string" },
        action: { type: "string", enum: ["open", "shell", "fork", "done"] },
        url: { type: "string" },
        command: { type: "string" },
        branches: { type: "array", items: { type: "string" } },
        summary: { type: "string" },
    },
    required: ["intent", "action"],
    additionalProperties: false,
};

await Deno.mkdir(`${run_dir}/frames`, { recursive: true });
const events = await Deno.open(`${run_dir}/events.jsonl`, {
    create: true,
    append: true,
});
const started_at = performance.now();
const encoder = new TextEncoder();

function emit(type: string, fields: Record<string, unknown> = {}): void {
    const line = JSON.stringify({
        t: Math.round(performance.now() - started_at),
        type,
        ...fields,
    });
    events.writeSync(encoder.encode(`${line}\n`));
    console.log(line.length > 240 ? `${line.slice(0, 240)}…` : line);
}

let frame_count = 0;

async function frame(lane: string, page: Page, label: string): Promise<void> {
    const file = `frames/${lane}-${String(++frame_count).padStart(3, "0")}.jpg`;
    await Deno.writeFile(`${run_dir}/${file}`, await page.jpeg(78));
    const url = await page.evaluate<string>("location.href");
    emit("frame", { lane, file, url, label });
}

type Lane = {
    id: string;
    machine: HostedMachine;
    browser: Browser;
    page: Page;
    agent: AgentSession<Step>;
};

type StepOutcome =
    | { kind: "continue"; message: string }
    | { kind: "fork"; branches: string[] }
    | { kind: "done" };

let issue_url: string | null = null;

async function act(lane: Lane, n: number, step: Step): Promise<StepOutcome> {
    emit("step", { lane: lane.id, n, ...step });
    if (step.action === "done") return { kind: "done" };
    if (step.action === "fork") {
        if (lane.id !== "main" || step.branches?.length !== copies) {
            return {
                kind: "continue",
                message: `Forking is not available now. Continue your work.`,
            };
        }
        return { kind: "fork", branches: step.branches };
    }
    if (step.action === "open" && step.url) {
        await lane.page.goto(step.url, 1500);
        await frame(lane.id, lane.page, "loaded");
        const read = await readPage(lane.page);
        for (const y of [700, 1400]) {
            if (read.height < y + 500) break;
            await lane.page.evaluate(
                `window.scrollTo({ top: ${y}, behavior: "instant" })`,
            );
            await delay(300);
            await frame(lane.id, lane.page, "reading");
        }
        emit("page", {
            lane: lane.id,
            n,
            url: read.url,
            title: read.title,
            chars: read.text.length,
        });
        return {
            kind: "continue",
            message:
                `Page: ${read.title}\nURL: ${read.url}\n\n${read.text}\n\nLinks:\n${
                    read.links.join("\n")
                }`,
        };
    }
    if (step.action === "shell" && step.command) {
        const result = await shell(
            lane.machine,
            step.command,
            60_000,
            "/workspace",
        );
        emit("shell", {
            lane: lane.id,
            n,
            exit_code: result.exit_code,
            ms: result.ms,
            output: tail(result.output, 6),
        });
        if (/api\.github\.com/.test(step.command)) {
            emit("egress", {
                lane: lane.id,
                host: "api.github.com",
                verdict: "inject",
            });
            const created = result.output.match(
                new RegExp(`https://github\\.com/${repository}/issues/\\d+`),
            );
            if (created && !issue_url) {
                issue_url = created[0];
                emit("issue", { lane: lane.id, n, url: issue_url });
            }
        }
        return {
            kind: "continue",
            message: `exit ${result.exit_code}\n${result.output.slice(-12000)}`,
        };
    }
    return { kind: "continue", message: "That action was missing a field." };
}

async function work(
    lane: Lane,
    first_message: string,
    max_steps: number,
    stop: () => boolean = () => false,
): Promise<StepOutcome> {
    let message = first_message;
    for (let n = 1; n <= max_steps; n++) {
        emit("thinking", { lane: lane.id, n });
        const { output: step } = await lane.agent.send(message);
        const outcome = await act(lane, n, step);
        if (outcome.kind !== "continue" || stop()) return outcome;
        message = outcome.message;
    }
    return { kind: "done" };
}

const token = new TextDecoder().decode(
    (await new Deno.Command("gh", { args: ["auth", "token"], stdout: "piped" })
        .output()).stdout,
).trim();
const host = new Host(machined_url, { apiKey: api_key });
const made: HostedMachine[] = [];
const browsers: Browser[] = [];
let snapshot_id: string | null = null;

async function openLane(
    id: string,
    machine: HostedMachine,
    agent: AgentSession<Step>,
    reuse_page: boolean,
): Promise<Lane> {
    const browser = await connectChromium(machine);
    browsers.push(browser);
    const page = reuse_page
        ? await browser.openedPage(viewport)
        : await browser.page(viewport);
    return { id, machine, browser, page, agent };
}

try {
    emit("run", { prompt, model, allowed_hosts, repository, copies });
    emit("usage_base", { ...(await host.usage()) });

    emit("scene", { id: "scout" });
    const create_started = performance.now();
    const created = await host.createMachine({
        memory_mib: 640,
        vcpus: 2,
        idle_timeout_secs: 3600,
        allowed_hosts,
        egress_credentials: [
            {
                host: "api.github.com",
                headers: { authorization: `Bearer ${token}` },
            },
        ],
    });
    made.push(created.machine);
    emit("machine", {
        lane: "main",
        id: created.machine.id,
        state: "running",
        boot_ms: created.info.boot_ms,
        ms: Math.round(performance.now() - create_started),
    });
    await startChromium(created.machine);
    const workdir = await Deno.makeTempDir({ prefix: "fork-research-agent-" });
    const main = await openLane(
        "main",
        created.machine,
        new AgentSession<Step>(
            model,
            system_prompt,
            step_schema,
            workdir,
            "medium",
        ),
        false,
    );
    emit("browser", { lane: "main" });
    await frame("main", main.page, "blank");

    const scouted = await work(main, prompt, 8);
    if (scouted.kind !== "fork") throw new Error("the agent did not fork");
    emit("fork_decision", { branches: scouted.branches });

    emit("scene", { id: "fork" });
    await main.browser.close();
    const pause_started = performance.now();
    await main.machine.pause();
    emit("machine", {
        lane: "main",
        state: "paused",
        ms: Math.round(performance.now() - pause_started),
    });
    const snapshot_started = performance.now();
    const snapshot = await main.machine.snapshot();
    snapshot_id = snapshot.id;
    emit("snapshot", {
        id: snapshot.id,
        stored_bytes: snapshot.stored_bytes,
        ms: Math.round(performance.now() - snapshot_started),
    });
    const fork_started = performance.now();
    const children = await Promise.all(
        scouted.branches.map(async (_, index) => {
            const child = await host.createMachine({
                snapshot_id: snapshot.id,
                idle_timeout_secs: 3600,
                egress_credentials: [],
            });
            made.push(child.machine);
            emit("machine", {
                lane: String(index + 1),
                id: child.machine.id,
                state: "running",
                ms: Math.round(performance.now() - fork_started),
            });
            return child.machine;
        }),
    );
    emit("fork", { ms: Math.round(performance.now() - fork_started) });
    const lanes = await Promise.all(children.map(async (machine, index) => {
        const lane = await openLane(
            String(index + 1),
            machine,
            main.agent.fork(),
            true,
        );
        await frame(lane.id, lane.page, "forked");
        return lane;
    }));

    emit("scene", { id: "parallel" });
    const sections = await Promise.all(lanes.map(async (lane, index) => {
        const started = performance.now();
        emit("branch_start", { lane: lane.id, topic: scouted.branches[index] });
        await work(
            lane,
            [
                `You are copy ${lane.id} of ${copies}. This computer is an exact copy of the original: same browser, same files.`,
                `Research only topic ${lane.id}: ${scouted.branches[index]}.`,
                `Read the E2B docs page for it, and check what machine-slop has today. Then write your section of the issue to /workspace/sections/${lane.id}.md (Markdown, a "###" heading, under 110 words, with the E2B docs link) and reply done.`,
            ].join("\n"),
            7,
        );
        const text = new TextDecoder().decode(
            await lane.machine.readFile(`/workspace/sections/${lane.id}.md`)
                .catch(() => new TextEncoder().encode("")),
        );
        emit("section", {
            lane: lane.id,
            text,
            ms: Math.round(performance.now() - started),
        });
        return text;
    }));

    emit("scene", { id: "merge" });
    for (const lane of lanes) {
        await lane.browser.close().catch(() => {});
        const destroy_started = performance.now();
        await lane.machine.destroy();
        emit("machine", {
            lane: lane.id,
            state: "destroyed",
            ms: Math.round(performance.now() - destroy_started),
        });
    }
    const resume_started = performance.now();
    await main.machine.resume();
    emit("machine", {
        lane: "main",
        state: "running",
        resumed: true,
        ms: Math.round(performance.now() - resume_started),
    });
    for (const [index, text] of sections.entries()) {
        await main.machine.writeFile(
            `/workspace/sections/${index + 1}.md`,
            text,
        );
    }
    emit("copied", { files: sections.length });
    const merged = await openLane("main", main.machine, main.agent, true);
    await frame("main", merged.page, "resumed");
    await work(
        merged,
        `Your ${copies} copies finished. Their sections are now in /workspace/sections/1.md to ${copies}.md on this computer. Combine them into one issue with a one-line intro, post it, and reply done.`,
        5,
        () => issue_url !== null,
    );

    if (issue_url) {
        emit("scene", { id: "result" });
        await merged.page.goto(issue_url, 2500);
        await frame("main", merged.page, "issue");
    }
    emit("usage", { ...(await host.usage()) });
    emit("done", { issue_url });
} finally {
    for (const browser of browsers) await browser.close().catch(() => {});
    if (Deno.env.get("KEEP_MACHINES") !== "1") {
        for (const machine of made) await machine.destroy().catch(() => {});
        if (snapshot_id) await host.deleteSnapshot(snapshot_id).catch(() => {});
    }
    events.close();
    console.log(`run recorded in ${run_dir}`);
}
