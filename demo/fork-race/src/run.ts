import {
    Host,
    type HostedMachine,
    tar,
} from "../../../packages/sdk/src/mod.ts";
import { AgentSession } from "./claude.ts";
import { Browser, delay, type Page } from "./browser.ts";
import { shell, type ShellResult, tail } from "./shell.ts";

const home = Deno.env.get("HOME")!;
const machined_url = Deno.env.get("MACHINED_URL") ?? "http://127.0.0.1:4100";
const api_key = (await Deno.readTextFile(
    Deno.env.get("API_KEY_FILE") ?? `${home}/machine-slop/demo/api-key`,
)).trim();
const chrome = Deno.env.get("CHROME")!;
const race_limit_ms = Number(Deno.env.get("RACE_LIMIT_MS") ?? 360_000);
const max_turns = 14;
const memory_mib = 384;
const demo_secret = "Bearer sk-demo-7c1e9a4f2b";
const app_dir = new URL("../app/", import.meta.url);
const run_dir = `${home}/machine-slop/demo/runs/${
    new Date().toISOString().replace(/[:.]/g, "-")
}`;

type Lane = {
    id: string;
    name: string;
    model: string;
    approach: string;
    hint: string;
};

const lanes: Lane[] = [
    {
        id: "A",
        name: "Opus 5.5",
        model: "claude-opus-5-5",
        approach: "Smallest diff",
        hint:
            "Make the smallest change that works: keep the logic in server.js and edit the existing page in place.",
    },
    {
        id: "B",
        name: "Sonnet 5.5",
        model: "claude-sonnet-5-5",
        approach: "UI component first",
        hint:
            "Start from the user experience: build the discount field as its own module, public/discount.js, then make the server satisfy it.",
    },
    {
        id: "C",
        name: "Haiku 5.5",
        model: "claude-haiku-5-5",
        approach: "Pricing module",
        hint:
            "Model the domain: put every discount rule in a new lib/discounts.js with a table of codes and expiry dates, then wire it into the server and page.",
    },
    {
        id: "D",
        name: "Fable 5.2",
        model: "claude-fable-5-2",
        approach: "Tests first",
        hint:
            "Work test-first: run the tests, fix one failing test at a time, and re-run them after each change.",
    },
];

const system_prompt = [
    "You are a coding agent working inside a Linux machine on a small Node.js 24 web app in /workspace/app, which is a git repository.",
    "The dev server is already running on port 3000 and reloads by itself when files change. Never start, stop or restart it.",
    "Each command runs in a fresh bash shell in /workspace/app; only files persist between commands.",
    "The files are small: read everything you need in one command, then make your changes. Write files with heredocs, for example cat > path <<'EOF'. Keep commands few and purposeful.",
    "Every reply is JSON. `intent` is one plain-English sentence of at most 12 words saying what you will do next and why, for a person watching. `commands` lists the shell commands to run now.",
    "When the task is complete, reply with an empty `commands` list and a one-sentence `summary`.",
].join("\n");

const task = [
    "Add discount codes to checkout.",
    "",
    "POST /api/checkout takes JSON { code } (optional) and returns the quote with `discount_cents`, `shipping_cents`, `total_cents` and the applied `code` in upper case.",
    "- SAVE20 takes 20% off the subtotal.",
    "- FREESHIP makes shipping free.",
    "- BULK3 takes $3 off each unit of any item bought in a quantity of 3 or more.",
    "- A blank code is the same as no code.",
    "- SUMMER10 has expired: respond 400 with { error } saying the code has expired.",
    '- Any other code: respond 400 with { error: "<CODE> is not a valid code" }.',
    "- Codes are case-insensitive and surrounding spaces are ignored.",
    "",
    'On the checkout page, add to the order summary an input with id="discount-code", a button with id="apply-discount", and a row with id="discount-line" that shows the discount. Applying a code updates the totals; an error shows inline.',
    "",
    "Run `npm test`; every test in test/ must pass. Do not edit the tests.",
    "After each of your turns a reviewer opens the live page in a browser, tries BOGUS and then SAVE20, and tells you what they saw.",
].join("\n");

await Deno.mkdir(`${run_dir}/shots`, { recursive: true });
const events = await Deno.open(`${run_dir}/events.jsonl`, {
    create: true,
    append: true,
});
const started_at = performance.now();
const encoder = new TextEncoder();

function emit(type: string, fields: Record<string, unknown>): void {
    const line = JSON.stringify({
        t: Math.round(performance.now() - started_at),
        type,
        ...fields,
    });
    events.writeSync(encoder.encode(`${line}\n`));
    console.log(line.length > 240 ? `${line.slice(0, 240)}…` : line);
}

const host = new Host(machined_url, { apiKey: api_key });
const browser = await Browser.launch(chrome);
const page = await browser.page({ width: 1000, height: 640 });
let page_queue = Promise.resolve();

function withPage<T>(work: (page: Page) => Promise<T>): Promise<T> {
    const result = page_queue.then(() => work(page));
    page_queue = result.then(() => {}, () => {});
    return result;
}

let shot_count = 0;

type BrowserCheck = { ok: boolean; seen: string };

async function capture(
    lane: string,
    url: string,
    try_discount: boolean,
): Promise<BrowserCheck | null> {
    return await withPage(async (page) => {
        await page.goto(url);
        const check = try_discount
            ? await page.evaluate<BrowserCheck>(`(async () => {
                const input = document.querySelector("#discount-code");
                const button = document.querySelector("#apply-discount");
                if (!input || !button) {
                    return { ok: false, seen: "there is no #discount-code input with an #apply-discount button on the page" };
                }
                const apply = async (code) => {
                    input.value = code;
                    input.dispatchEvent(new Event("input", { bubbles: true }));
                    button.click();
                    await new Promise((resolve) => setTimeout(resolve, 900));
                };
                await apply("BOGUS");
                const error_shown = /not a valid code/i.test(document.body.innerText);
                await apply("SAVE20");
                const text = (selector) => document.querySelector(selector)?.textContent?.trim() ?? "(missing)";
                const total = text("#total");
                const line = text("#discount-line");
                const ok = error_shown && total.includes("69.00") && line.includes("16.00");
                return { ok, seen: (error_shown ? "BOGUS showed an inline error" : "BOGUS showed no 'not a valid code' error on the page") + "; with SAVE20, #total shows " + JSON.stringify(total) + " and #discount-line shows " + JSON.stringify(line) };
            })()`).catch((error) => ({ ok: false, seen: String(error) }))
            : null;
        const file = `shots/${lane}-${
            String(++shot_count).padStart(3, "0")
        }.jpg`;
        await Deno.writeFile(`${run_dir}/${file}`, await page.jpeg());
        emit("shot", { lane, file, applied: check !== null });
        if (check) emit("browser", { lane, ok: check.ok, seen: check.seen });
        return check;
    }).catch((error) => {
        emit("note", { lane, text: `screenshot failed: ${error}` });
        return null;
    });
}

async function devInfo(lane: string, url: string): Promise<void> {
    try {
        const response = await fetch(new URL("/__dev", url));
        const info = await response.json();
        emit("dev", { lane, pid: info.pid, uptime_ms: info.uptime_ms });
    } catch (error) {
        emit("note", { lane, text: `dev info failed: ${error}` });
    }
}

const test_count = 11;

function testResults(result: ShellResult): boolean[] {
    const outcomes = [...result.output.matchAll(/^(not ok|ok) \d+ - /gm)].map((
        match,
    ) => match[1] === "ok");
    return Array.from(
        { length: test_count },
        (_, index) => outcomes[index] ?? false,
    );
}

async function runTests(lane: string, machine: HostedMachine): Promise<number> {
    const result = await shell(
        machine,
        "node --test --test-reporter=tap 2>&1",
        60_000,
    );
    const results = testResults(result);
    const passed = results.filter(Boolean).length;
    emit("tests", { lane, results, passed });
    return passed;
}

async function diffStat(lane: string, machine: HostedMachine): Promise<void> {
    const result = await shell(
        machine,
        "git add -A -N . && git diff --numstat",
        20_000,
    );
    const files = result.output.trim().split("\n").filter(Boolean).map(
        (line) => {
            const [added, removed, path] = line.split("\t");
            return { path, added: Number(added), removed: Number(removed) };
        },
    );
    emit("diff", { lane, files });
}

const denied_hosts = new Map<string, Set<string>>();

function noticeEgress(lane: string, output: string): void {
    if (!/response 403|E403|403 Forbidden/.test(output)) {
        return;
    }
    const hosts = output.match(
        /\b(?:[a-z0-9-]+\.)+(?:org|com|net|io|dev|app)\b/g,
    ) ?? [];
    const seen = denied_hosts.get(lane) ?? new Set<string>();
    denied_hosts.set(lane, seen);
    for (const name of hosts) {
        if (seen.has(name)) continue;
        seen.add(name);
        emit("egress", {
            lane,
            verdict: "deny",
            host: name,
            detail: "not on this machine's allowlist",
        });
    }
}

async function runCommand(
    lane: string,
    machine: HostedMachine,
    command: string,
    timeout_ms = 90_000,
): Promise<ShellResult> {
    emit("status", { lane, status: "running", detail: command.split("\n")[0] });
    const result = await shell(machine, command, timeout_ms);
    emit("command", {
        lane,
        cmd: command,
        exit_code: result.exit_code,
        timed_out: result.timed_out,
        ms: result.ms,
        output: tail(result.output, 5),
    });
    noticeEgress(lane, `${command}\n${result.output}`);
    return result;
}

const created: HostedMachine[] = [];
let snapshot_id: string | null = null;

async function usage(): Promise<void> {
    const recorded = await host.usage();
    emit("usage", {
        vcpu_ms: recorded.vcpu_ms,
        memory_mib_ms: recorded.memory_mib_ms,
        snapshot_mib_ms: recorded.snapshot_mib_ms,
    });
}

try {
    emit("run", { lanes, task, memory_mib, machined: "local" });
    const before_usage = await host.usage();
    emit("usage_base", {
        vcpu_ms: before_usage.vcpu_ms,
        memory_mib_ms: before_usage.memory_mib_ms,
        snapshot_mib_ms: before_usage.snapshot_mib_ms,
    });

    emit("scene", { id: "boot" });
    const create_started = performance.now();
    const { machine: template, info } = await host.createMachine({
        memory_mib,
        vcpus: 2,
        idle_timeout_secs: 3600,
        allowed_hosts: ["httpbin.org", "postman-echo.com"],
        egress_credentials: [
            { host: "httpbin.org", headers: { authorization: demo_secret } },
            {
                host: "postman-echo.com",
                headers: { authorization: demo_secret },
            },
        ],
    });
    created.push(template);
    emit("machine", {
        lane: "template",
        id: template.id,
        state: "running",
        boot_ms: info.boot_ms,
        ms: Math.round(performance.now() - create_started),
    });

    emit("scene", { id: "setup" });
    const files: Record<string, string> = {};
    for (
        const path of [
            "package.json",
            "server.js",
            "lib/cart.js",
            "public/index.html",
            "public/app.js",
            "public/style.css",
            "test/checkout.test.js",
        ]
    ) {
        files[path] = await Deno.readTextFile(new URL(path, app_dir));
    }
    const upload_started = performance.now();
    await template.writeArchive("/workspace/app", tar(files));
    emit("upload", {
        lane: "template",
        files: Object.keys(files).length,
        bytes: Object.values(files).reduce((sum, text) => sum + text.length, 0),
        ms: Math.round(performance.now() - upload_started),
    });
    await runCommand(
        "template",
        template,
        "git init -q && git add -A && git -c user.email=demo@slop.dev -c user.name=demo commit -qm 'slop supply checkout' && git log --oneline",
    );
    await template.startProcess({
        program: "npm",
        args: ["run", "dev"],
        cwd: "/workspace/app",
    });
    emit("command", {
        lane: "template",
        cmd: "npm run dev &",
        exit_code: null,
        ms: 0,
        output: ["slop supply on :3000"],
    });
    await runCommand(
        "template",
        template,
        "for i in $(seq 1 50); do curl -sf localhost:3000/api/cart >/dev/null && break; sleep 0.1; done; curl -s localhost:3000/__dev",
    );
    const template_preview = await template.preview(3000, { ttl_secs: 7200 });
    emit("preview", { lane: "template", url: template_preview.url });
    await capture("template", template_preview.url, false);
    await runTests("template", template);

    emit("scene", { id: "secrets" });
    await runCommand(
        "template",
        template,
        "env | grep -ci 'sk-demo' || echo 'no secret anywhere in the environment'",
    );
    const echoed = await runCommand(
        "template",
        template,
        'curl -s --max-time 15 https://httpbin.org/headers | grep -i authorization || curl -s --max-time 15 https://postman-echo.com/headers | grep -o \'"authorization":"[^"]*"\'',
    );
    await runCommand(
        "template",
        template,
        "curl -sS --max-time 10 -o /dev/null https://example.com 2>&1 || true",
    );
    emit("egress", {
        lane: "template",
        verdict: /sk-demo/.test(echoed.output) ? "inject" : "deny",
        host: /postman/.test(echoed.output)
            ? "postman-echo.com"
            : "httpbin.org",
        detail: "Authorization header added outside the machine",
    });
    await devInfo("template", template_preview.url);

    emit("scene", { id: "snapshot" });
    const snapshot_started = performance.now();
    const snapshot = await template.snapshot();
    snapshot_id = snapshot.id;
    emit("snapshot", {
        id: snapshot.id,
        stored_bytes: snapshot.stored_bytes,
        ms: Math.round(performance.now() - snapshot_started),
    });
    const park_started = performance.now();
    await template.pause();
    emit("machine", {
        lane: "template",
        id: template.id,
        state: "paused",
        ms: Math.round(performance.now() - park_started),
    });

    emit("scene", { id: "fork" });
    const fork_started = performance.now();
    const children = await Promise.all(lanes.map(async (lane) => {
        const { machine, info } = await host.createMachine({
            snapshot_id: snapshot.id,
            idle_timeout_secs: 3600,
            allowed_hosts: [],
        });
        created.push(machine);
        emit("machine", {
            lane: lane.id,
            id: machine.id,
            state: "running",
            boot_ms: info.boot_ms,
            ms: Math.round(performance.now() - fork_started),
            parent: snapshot.id,
        });
        return machine;
    }));
    emit("fork", {
        ms: Math.round(performance.now() - fork_started),
        count: children.length,
    });
    const previews = await Promise.all(
        children.map((machine) => machine.preview(3000, { ttl_secs: 7200 })),
    );
    for (const [index, lane] of lanes.entries()) {
        emit("preview", { lane: lane.id, url: previews[index]!.url });
        await devInfo(lane.id, previews[index]!.url);
    }
    for (const [index, lane] of lanes.entries()) {
        await capture(lane.id, previews[index]!.url, false);
    }

    emit("scene", { id: "race" });
    const race_started = performance.now();
    let winner: string | null = null;
    const workdir = await Deno.makeTempDir({ prefix: "fork-race-agents-" });

    const raceLane = async (
        lane: Lane,
        machine: HostedMachine,
        url: string,
    ) => {
        emit("agent", {
            lane: lane.id,
            name: lane.name,
            approach: lane.approach,
        });
        const session = new AgentSession(lane.model, system_prompt, workdir);
        let message = `${task}\n\nYour approach: ${lane.hint}`;
        for (let turn = 1; turn <= max_turns; turn++) {
            if (performance.now() - race_started > race_limit_ms) {
                emit("status", { lane: lane.id, status: "out of time" });
                return;
            }
            emit("status", { lane: lane.id, status: "thinking", turn });
            const reply = await session.send(message).catch((error) => {
                emit("note", { lane: lane.id, text: String(error) });
                return null;
            });
            if (!reply) return;
            emit("intent", {
                lane: lane.id,
                turn,
                text: reply.intent,
                tokens: reply.output_tokens,
            });
            if (reply.commands.length === 0) {
                emit("status", {
                    lane: lane.id,
                    status: "finished",
                    detail: reply.summary,
                });
                return;
            }
            const outputs: string[] = [];
            for (const command of reply.commands) {
                const result = await runCommand(lane.id, machine, command);
                outputs.push(
                    `Output of \`${
                        command.split("\n")[0]
                    }\` (exit ${result.exit_code}${
                        result.timed_out ? ", timed out" : ""
                    }):\n${result.output.slice(-16000)}`,
                );
            }
            emit("status", { lane: lane.id, status: "testing" });
            const [passed] = await Promise.all([
                runTests(lane.id, machine),
                diffStat(lane.id, machine),
            ]);
            const check = await capture(lane.id, url, true);
            if (passed === test_count && check?.ok) {
                const ms = Math.round(performance.now() - race_started);
                if (!winner) {
                    winner = lane.id;
                    emit("winner", { lane: lane.id, turn, ms });
                }
                emit("green", { lane: lane.id, turn, ms });
                emit("status", { lane: lane.id, status: "green" });
                return;
            }
            const review = check
                ? `Reviewer, in the browser: ${check.seen}. Expected an inline error for BOGUS, then a total of $69.00 and a $16.00 discount for SAVE20.`
                : "Reviewer: the page could not be checked.";
            message = [...outputs, review].join("\n\n");
        }
        emit("status", { lane: lane.id, status: "out of turns" });
    };

    await Promise.all(
        lanes.map((lane, index) =>
            raceLane(lane, children[index]!, previews[index]!.url)
        ),
    );
    emit("race_end", {
        winner,
        ms: Math.round(performance.now() - race_started),
    });

    emit("scene", { id: "pick" });
    for (const [index, lane] of lanes.entries()) {
        if (lane.id === winner) continue;
        const destroy_started = performance.now();
        await children[index]!.destroy();
        emit("machine", {
            lane: lane.id,
            id: children[index]!.id,
            state: "destroyed",
            ms: Math.round(performance.now() - destroy_started),
        });
        await delay(250);
    }

    const winner_index = lanes.findIndex((lane) => lane.id === winner);
    if (winner_index >= 0) {
        const machine = children[winner_index]!;
        const url = previews[winner_index]!.url;
        await usage();
        emit("scene", { id: "pause" });
        await devInfo(winner!, url);
        const pause_started = performance.now();
        await machine.pause();
        emit("machine", {
            lane: winner,
            id: machine.id,
            state: "paused",
            ms: Math.round(performance.now() - pause_started),
        });
        const parked = await fetch(url).then((response) => response.status)
            .catch(() => 0);
        emit("note", {
            lane: winner,
            text: `preview while paused: HTTP ${parked}`,
        });
        await delay(4000);
        const resume_started = performance.now();
        await machine.resume();
        emit("machine", {
            lane: winner,
            id: machine.id,
            state: "running",
            resumed: true,
            ms: Math.round(performance.now() - resume_started),
        });
        await devInfo(winner!, url);
        await capture(winner!, url, true);
    }

    emit("scene", { id: "bill" });
    await usage();
    emit("done", {});
} finally {
    await browser.close().catch(() => {});
    if (Deno.env.get("KEEP_MACHINES") !== "1") {
        for (const machine of created) {
            await machine.destroy().catch(() => {});
        }
        if (snapshot_id) await host.deleteSnapshot(snapshot_id).catch(() => {});
    }
    events.close();
    console.log(`run recorded in ${run_dir}`);
}
