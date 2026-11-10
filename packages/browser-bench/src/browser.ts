import { Cdp, type CdpSession, withDeadline } from "./cdp.ts";

export type BrowserProfile = {
    name: string;
    executable: string;
    product: string;
    args: string[];
};

export type Startup = { devtoolsMs: number; usablePageMs: number };

export const viewport = { width: 1280, height: 800, deviceScaleFactor: 1 };

export const pinnedArgs = [
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

export const pinnedEnvironment = { TZ: "UTC" };

const startupTimeoutMs = 30_000;

export class PinMismatch extends Error {
    constructor(profile: BrowserProfile, actual: string) {
        super(
            `browser "${profile.name}" reports ${actual}, pinned ${profile.product}`,
        );
        this.name = "PinMismatch";
    }
}

export class Browser {
    private constructor(
        readonly pid: number,
        readonly page: CdpSession,
        readonly startup: Startup,
        readonly product: string,
        private readonly cdp: Cdp,
        private readonly child: Deno.ChildProcess,
        private readonly profileDirectory: string,
        private readonly stderrDrained: Promise<void>,
    ) {}

    static async launch(profile: BrowserProfile): Promise<Browser> {
        const profileDirectory = await Deno.makeTempDir({
            prefix: "browser-bench-",
        });
        const spawnedAt = performance.now();
        const child = new Deno.Command(profile.executable, {
            args: [
                ...pinnedArgs,
                ...profile.args,
                "--remote-debugging-port=0",
                `--user-data-dir=${profileDirectory}`,
                "about:blank",
            ],
            env: pinnedEnvironment,
            stdin: "null",
            stdout: "null",
            stderr: "piped",
        }).spawn();

        const endpoint = Promise.withResolvers<string>();
        endpoint.promise.catch(() => {});
        const stderrDrained = drainStderr(child.stderr, endpoint);
        try {
            const url = await withDeadline(
                "browser startup",
                startupTimeoutMs,
                endpoint.promise,
            );
            const devtoolsMs = performance.now() - spawnedAt;
            const cdp = await Cdp.connect(url);
            const { product } = await cdp.send<{ product: string }>(
                "Browser.getVersion",
            );
            if (product !== profile.product) {
                throw new PinMismatch(profile, product);
            }
            const page = await withDeadline(
                "first page",
                startupTimeoutMs,
                attachFirstPage(cdp),
            );
            return new Browser(
                child.pid,
                page,
                { devtoolsMs, usablePageMs: performance.now() - spawnedAt },
                product,
                cdp,
                child,
                profileDirectory,
                stderrDrained,
            );
        } catch (error) {
            await terminate(child, stderrDrained, profileDirectory);
            throw error;
        }
    }

    async close(): Promise<void> {
        this.cdp.close();
        await terminate(this.child, this.stderrDrained, this.profileDirectory);
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
        endpoint.reject(
            new Error(`browser exited before DevTools was ready: ${head}`),
        );
    }
}

async function terminate(
    child: Deno.ChildProcess,
    stderrDrained: Promise<void>,
    profileDirectory: string,
): Promise<void> {
    try {
        child.kill("SIGKILL");
    } catch (error) {
        if (!(error instanceof TypeError)) throw error;
    }
    await child.status;
    await stderrDrained.catch(() => {});
    await Deno.remove(profileDirectory, { recursive: true }).catch(() => {});
}
