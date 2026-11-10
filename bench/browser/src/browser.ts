import { Cdp, type CdpSession, withDeadline } from "./cdp.ts";

export type BrowserProfile = {
    name: string;
    executable: string;
    product: string;
    args: string[];
};

export type Startup = { devtools_ms: number; usable_page_ms: number };

export const viewport = { width: 1280, height: 800, deviceScaleFactor: 1 };

export const pinned_args = [
    "--headless=new",
    `--window-size=${viewport.width},${viewport.height}`,
    `--force-device-scale-factor=${viewport.deviceScaleFactor}`,
    "--lang=en-US",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-field-trial-config",
    "--disable-background-networking",
    "--disable-component-update",
    "--disable-sync",
    "--disable-default-apps",
    "--disable-extensions",
    "--disable-search-engine-choice-screen",
    "--password-store=basic",
    "--use-mock-keychain",
    "--mute-audio",
];

export const pinned_environment = { TZ: "UTC" };

const startup_timeout_ms = 30_000;

export class PinMismatch extends Error {
    constructor(profile: BrowserProfile, actual: string) {
        super(
            `browser "${profile.name}" reports ${actual}, pinned ${profile.product}`,
        );
        this.name = "PinMismatch";
    }
}

export class BrowserExited extends Error {
    constructor(stderr: string) {
        super(`browser exited before DevTools was ready: ${stderr}`);
        this.name = "BrowserExited";
    }
}

export class Browser {
    private constructor(
        readonly pid: number,
        readonly page: CdpSession,
        readonly startup: Startup,
        readonly endpoint: string,
        private readonly cdp: Cdp,
        private readonly child: Deno.ChildProcess,
        private readonly profile_directory: string,
        private readonly stderr_drained: Promise<void>,
    ) {}

    static async launch(profile: BrowserProfile): Promise<Browser> {
        const profile_directory = await Deno.makeTempDir({
            prefix: "browser-bench-",
        });
        const spawned_at = performance.now();
        const child = new Deno.Command(profile.executable, {
            args: [
                ...pinned_args,
                ...profile.args,
                "--remote-debugging-port=0",
                `--user-data-dir=${profile_directory}`,
                "about:blank",
            ],
            env: pinned_environment,
            stdin: "null",
            stdout: "null",
            stderr: "piped",
        }).spawn();

        const endpoint = Promise.withResolvers<string>();
        endpoint.promise.catch(() => {});
        const stderr_drained = drainStderr(child.stderr, endpoint);
        try {
            const url = await withDeadline(
                "browser startup",
                startup_timeout_ms,
                endpoint.promise,
            );
            const devtools_ms = performance.now() - spawned_at;
            const cdp = await Cdp.connect(url);
            const { product } = await cdp.send<{ product: string }>(
                "Browser.getVersion",
            );
            if (product !== profile.product) {
                throw new PinMismatch(profile, product);
            }
            const page = await withDeadline(
                "first page",
                startup_timeout_ms,
                attachFirstPage(cdp),
            );
            return new Browser(
                child.pid,
                page,
                { devtools_ms, usable_page_ms: performance.now() - spawned_at },
                url,
                cdp,
                child,
                profile_directory,
                stderr_drained,
            );
        } catch (error) {
            await terminate(child, stderr_drained, profile_directory);
            throw error;
        }
    }

    async close(): Promise<void> {
        this.cdp.close();
        await terminate(
            this.child,
            this.stderr_drained,
            this.profile_directory,
        );
    }
}

async function attachFirstPage(cdp: Cdp): Promise<CdpSession> {
    while (true) {
        const { targetInfos } = await cdp.send<{
            targetInfos: { targetId: string; type: string }[];
        }>("Target.getTargets");
        const target = targetInfos.find((info) => info.type === "page");
        if (target) {
            const { sessionId } = await cdp.send<{ sessionId: string }>(
                "Target.attachToTarget",
                { targetId: target.targetId, flatten: true },
            );
            const page = cdp.session(sessionId);
            await page.send("Runtime.evaluate", { expression: "1" });
            return page;
        }
        await new Promise((resolve) => setTimeout(resolve, 5));
    }
}

async function drainStderr(
    stderr: ReadableStream<Uint8Array>,
    endpoint: PromiseWithResolvers<string>,
): Promise<void> {
    const decoder = new TextDecoder();
    let head = "";
    let found = false;
    for await (const chunk of stderr) {
        if (found) continue;
        head += decoder.decode(chunk, { stream: true });
        const url = head.match(/DevTools listening on (ws:\/\/\S+)/)?.[1];
        if (url) {
            found = true;
            endpoint.resolve(url);
        }
    }
    if (!found) {
        endpoint.reject(new BrowserExited(head));
    }
}

async function terminate(
    child: Deno.ChildProcess,
    stderr_drained: Promise<void>,
    profile_directory: string,
): Promise<void> {
    try {
        child.kill("SIGKILL");
    } catch (error) {
        if (!(error instanceof TypeError)) throw error;
    }
    await child.status;
    await stderr_drained.catch(() => {});
    await Deno.remove(profile_directory, { recursive: true }).catch(() => {});
}
