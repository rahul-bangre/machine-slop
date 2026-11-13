import { Host, type HostedMachine } from "../../../packages/sdk/src/mod.ts";
import { AgentSession } from "../../common/claude.ts";
import { Browser, delay, type Page } from "../../common/browser.ts";
import { shell, tail } from "../../common/shell.ts";
import {
    connectChromium,
    type PageText,
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
const max_steps = 18;
const run_dir = `${home}/machine-slop/demo/runs/github-${
    new Date().toISOString().replace(/[:.]/g, "-")
}`;

const prompt =
    `Read the ${repository} repo on GitHub to understand what it does, then read E2B's docs. Open an issue on the repo listing the 3 most important things E2B can do that machine-slop can't yet, with a link to the E2B docs for each.`;

const allowed_hosts = [
    "github.com",
    "*.github.com",
    "*.githubusercontent.com",
    "*.githubassets.com",
    "e2b.dev",
    "*.e2b.dev",
];

const system_prompt = [
    "You are an agent with a fresh Linux computer and a web browser.",
    "Every reply is JSON. `intent` is one plain-English sentence of at most 12 words saying what you will do next and why, for a person watching your screen.",
    '`action` is "open" to load `url` in the browser (you get back the page text and its links), "shell" to run `command` with bash in /workspace, or "done" with a one-sentence `summary` when the task is finished.',
    "Read with the browser: open GitHub pages and files and E2B docs pages to see their text and links. Use the shell only to create the issue.",
    "The internet is limited to github.com and e2b.dev and their subdomains.",
    `To create an issue, POST JSON to https://api.github.com/repos/${repository}/issues with curl: write the body to a file first (cat > issue.json <<'EOF' ... EOF), then curl -sS -X POST -H 'Accept: application/vnd.github+json' --data @issue.json <url>. You have no GitHub token and need none: the network adds authentication for api.github.com.`,
    'Start the issue title with "Gaps vs E2B:". Keep the issue under 250 words, in Markdown.',
    "Be efficient: aim for about 8 steps.",
].join("\n");

type Step = {
    intent: string;
    action: "open" | "shell" | "done";
    url?: string;
    command?: string;
    summary?: string;
};

const step_schema = {
    type: "object",
    properties: {
        intent: { type: "string" },
        action: { type: "string", enum: ["open", "shell", "done"] },
        url: { type: "string" },
        command: { type: "string" },
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

async function frame(page: Page, label: string): Promise<void> {
    const file = `frames/${String(++frame_count).padStart(3, "0")}.jpg`;
    await Deno.writeFile(`${run_dir}/${file}`, await page.jpeg(80));
    const url = await page.evaluate<string>("location.href");
    emit("frame", { file, url, label });
}

async function openPage(page: Page, url: string): Promise<PageText> {
    await page.goto(url, 1500);
    await frame(page, "loaded");
    const read = await readPage(page);
    for (const y of [700, 1400]) {
        if (read.height < y + 500) break;
        await page.evaluate(
            `window.scrollTo({ top: ${y}, behavior: "instant" })`,
        );
        await delay(350);
        await frame(page, "reading");
    }
    return read;
}

async function startBrowser(machine: HostedMachine): Promise<Browser> {
    await startChromium(machine);
    return await connectChromium(machine);
}

const token = new TextDecoder().decode(
    (await new Deno.Command("gh", { args: ["auth", "token"], stdout: "piped" })
        .output()).stdout,
).trim();
const host = new Host(machined_url, { apiKey: api_key });
let machine: HostedMachine | null = null;
let browser: Browser | null = null;

try {
    emit("run", { prompt, model, allowed_hosts, repository });
    const base_usage = await host.usage();
    emit("usage_base", { ...base_usage });

    emit("scene", { id: "boot" });
    const create_started = performance.now();
    const created = await host.createMachine({
        memory_mib: 1536,
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
    machine = created.machine;
    emit("machine", {
        id: machine.id,
        state: "running",
        boot_ms: created.info.boot_ms,
        ms: Math.round(performance.now() - create_started),
    });
    const browser_started = performance.now();
    browser = await startBrowser(machine);
    const page = await browser.page({ width: 1280, height: 800 });
    emit("browser", { ms: Math.round(performance.now() - browser_started) });
    await frame(page, "blank");

    emit("scene", { id: "work" });
    const workdir = await Deno.makeTempDir({ prefix: "github-issue-agent-" });
    const agent = new AgentSession<Step>(
        model,
        system_prompt,
        step_schema,
        workdir,
        "medium",
    );
    let message = prompt;
    let issue_url: string | null = null;
    for (let n = 1; n <= max_steps; n++) {
        emit("thinking", { n });
        const { output: step, output_tokens } = await agent.send(message);
        emit("step", { n, ...step, output_tokens });
        if (step.action === "done") break;
        if (step.action === "open" && step.url) {
            const read = await openPage(page, step.url);
            emit("page", {
                n,
                url: read.url,
                title: read.title,
                chars: read.text.length,
                links: read.links.length,
            });
            message =
                `Page: ${read.title}\nURL: ${read.url}\n\n${read.text}\n\nLinks:\n${
                    read.links.join("\n")
                }`;
            continue;
        }
        if (step.action === "shell" && step.command) {
            const result = await shell(
                machine,
                step.command,
                60_000,
                "/workspace",
            );
            emit("shell", {
                n,
                exit_code: result.exit_code,
                ms: result.ms,
                output: tail(result.output, 6),
            });
            const created_issue = result.output.match(
                new RegExp(
                    `https://github\\.com/${repository}/issues/\\d+`,
                ),
            );
            if (/api\.github\.com/.test(step.command)) {
                emit("egress", {
                    host: "api.github.com",
                    verdict: "inject",
                    token_in_machine: false,
                });
            }
            if (created_issue && !issue_url) {
                issue_url = created_issue[0];
                const title =
                    result.output.match(/"title":\s*"([^"]*)"/)?.[1] ??
                        null;
                emit("issue", { n, url: issue_url, title });
            }
            message = `exit ${result.exit_code}\n${
                result.output.slice(-12000)
            }`;
            continue;
        }
        message = "That action was missing its url or command.";
    }

    if (issue_url) {
        emit("scene", { id: "result" });
        await page.goto(issue_url, 2500);
        await frame(page, "issue");
        await page.evaluate(
            `window.scrollTo({ top: 420, behavior: "instant" })`,
        );
        await delay(400);
        await frame(page, "issue body");
    }
    const usage = await host.usage();
    emit("usage", { ...usage });
    emit("done", { issue_url });
} finally {
    await browser?.close().catch(() => {});
    if (machine && Deno.env.get("KEEP_MACHINE") !== "1") {
        await machine.destroy().catch(() => {});
    }
    events.close();
    console.log(`run recorded in ${run_dir}`);
}
