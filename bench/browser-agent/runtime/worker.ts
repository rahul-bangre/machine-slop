import { Browser, type BrowserProfile } from "../../browser/src/browser.ts";
import { Page } from "../../browser/src/page.ts";
import { UsageMeter } from "../../browser/src/usage.ts";
import { connectNative } from "./native.ts";
import { connectPlaywright } from "./playwright.ts";
import { Session } from "./session.ts";
import { browserTask, openTask } from "./tasks.ts";

export type RuntimeConfig = {
    engine: "native" | "playwright";
    task: string;
    seed?: number;
    browser: BrowserProfile;
};

export async function serve(config: RuntimeConfig, control_path: string) {
    const task = browserTask(config.task);
    const seed = config.seed ?? task.seeds[0]!;
    const site = openTask(task, seed);
    const browser = await Browser.launch(config.browser).catch(
        async (error) => {
            await site.stop();
            throw error;
        },
    );
    const lifetime = { closed: false };
    const driver =
        await (config.engine === "native"
            ? connectNative(browser)
            : connectPlaywright(browser.endpoint)).catch(async (error) => {
                await browser.close();
                await site.stop();
                throw error;
            });
    const usage = new UsageMeter(Deno.pid);
    const environment = {
        os: Deno.build.os,
        arch: Deno.build.arch,
        deno: Deno.version,
        hardware_concurrency: navigator.hardwareConcurrency,
        cgroup: Object.fromEntries(
            await Promise.all(
                ["cpu.max", "memory.max", "cpuset.cpus.effective"].map(async (
                    name,
                ) => [
                    name,
                    await Deno.readTextFile(`/sys/fs/cgroup/${name}`).then(
                        (text) => text.trim(),
                        () => null,
                    ),
                ]),
            ),
        ),
    };
    const sampling = {
        pending: Promise.resolve(),
        error: null as string | null,
    };
    const timer = setInterval(() => {
        sampling.pending = sampling.pending.then(() => usage.sample()).catch(
            (error) => {
                sampling.error = String(error);
            },
        );
    }, 250);
    const closeBrowser = async () => {
        if (lifetime.closed) return;
        lifetime.closed = true;
        await browser.close();
    };
    const session = new Session(driver, closeBrowser);
    const token = crypto.randomUUID();
    const cleanup: { promise: Promise<void> | null } = { promise: null };
    const clean = async () => {
        clearInterval(timer);
        await sampling.pending;
        await driver.close().catch(() => {});
        await closeBrowser();
        await site.stop();
    };
    const close = () => cleanup.promise ??= clean();
    try {
        const page = await Page.prepare(browser.page);
        await page.act({ goto: site.origin });
        await page.settle("true", 10_000);
        const server = Deno.serve({
            hostname: "127.0.0.1",
            port: 0,
            onListen() {},
        }, async (request) => {
            if (request.headers.get("authorization") !== `Bearer ${token}`) {
                return new Response(null, { status: 401 });
            }
            const path = new URL(request.url).pathname;
            if (request.method !== "POST") {
                return new Response(null, { status: 405 });
            }
            if (path === "/command") {
                try {
                    const text = await request.text();
                    if (text.length > 65_536) {
                        return new Response("command too large", {
                            status: 413,
                        });
                    }
                    return Response.json(
                        await session.command(JSON.parse(text)),
                    );
                } catch (error) {
                    return Response.json({ error: String(error) }, {
                        status: 400,
                    });
                }
            }
            if (path === "/report") {
                await sampling.pending;
                await usage.sample().catch((error) =>
                    sampling.error = String(error)
                );
                return Response.json({
                    engine: config.engine,
                    browser: config.browser,
                    startup: browser.startup,
                    task: task.id,
                    goal: site.prompt,
                    seed,
                    partition: task.partition,
                    environment,
                    checker_failure: await site.outcome(),
                    commands: session.records,
                    usage: {
                        cpu_seconds: await usage.cpuSecondsNow().catch(
                            (error) => {
                                sampling.error = String(error);
                                return null;
                            },
                        ),
                        peak_rss_sum_bytes: usage.peakRssSumBytes(),
                        sample_interval_ms: 250,
                        error: sampling.error,
                        caveat:
                            "runtime and browser descendants, including fixture, tokenization, sampler and idle time; summed RSS double-counts shared pages; CPU misses processes that exit between samples",
                    },
                    tokenizer:
                        "o200k_base (gpt-tokenizer 4.1.0); proxy, not billed model tokens",
                });
            }
            if (path === "/stop") {
                setTimeout(() => {
                    close().then(() => server.shutdown()).catch(console.error);
                }, 0);
                return Response.json({ stopping: true });
            }
            return new Response(null, { status: 404 });
        });
        await Deno.writeTextFile(
            control_path,
            JSON.stringify({ port: server.addr.port, token }),
            { mode: 0o600 },
        );
        const expiry = setTimeout(() => {
            close().then(() => server.shutdown()).catch(console.error);
        }, 30 * 60_000);
        await server.finished.finally(() => clearTimeout(expiry));
    } finally {
        await close();
        await Deno.remove(control_path).catch(() => {});
    }
}

if (import.meta.main) {
    const config: RuntimeConfig = JSON.parse(
        await Deno.readTextFile(Deno.args[0]!),
    );
    await serve(config, Deno.args[1]!);
}
