import { Cdp, type CdpSession } from "../../../bench/browser/src/cdp.ts";

export type Viewport = { width: number; height: number };

export class Browser {
    private constructor(
        private readonly child: Deno.ChildProcess,
        private readonly profile: string,
        readonly cdp: Cdp,
    ) {}

    static async launch(executable: string): Promise<Browser> {
        const profile = await Deno.makeTempDir({ prefix: "fork-race-chrome-" });
        const child = new Deno.Command(executable, {
            args: [
                "--headless=new",
                "--no-sandbox",
                "--disable-gpu",
                "--hide-scrollbars",
                "--mute-audio",
                "--no-first-run",
                "--remote-debugging-port=0",
                `--user-data-dir=${profile}`,
                "about:blank",
            ],
            stdout: "null",
            stderr: "piped",
        }).spawn();
        const reader = child.stderr.pipeThrough(new TextDecoderStream())
            .getReader();
        let seen = "";
        while (true) {
            const { value, done } = await reader.read();
            if (done) throw new Error(`chrome exited: ${seen.slice(-400)}`);
            seen += value;
            const match = seen.match(/DevTools listening on (ws:\S+)/);
            if (match?.[1]) {
                reader.releaseLock();
                child.stderr.pipeTo(new WritableStream()).catch(() => {});
                return new Browser(child, profile, await Cdp.connect(match[1]));
            }
        }
    }

    async page(viewport: Viewport): Promise<Page> {
        const { targetId } = await this.cdp.send<{ targetId: string }>(
            "Target.createTarget",
            { url: "about:blank" },
        );
        const { sessionId } = await this.cdp.send<{ sessionId: string }>(
            "Target.attachToTarget",
            { targetId, flatten: true },
        );
        const session = this.cdp.session(sessionId);
        await session.send("Page.enable");
        await session.send("Runtime.enable");
        await session.send("Emulation.setDeviceMetricsOverride", {
            ...viewport,
            deviceScaleFactor: 1,
            mobile: false,
        });
        return new Page(session);
    }

    async close(): Promise<void> {
        this.cdp.close();
        try {
            this.child.kill("SIGKILL");
        } catch {
            return;
        }
        await this.child.status;
        await Deno.remove(this.profile, { recursive: true }).catch(() => {});
    }
}

export class Page {
    constructor(readonly session: CdpSession) {}

    async goto(url: string, settle_ms = 600): Promise<void> {
        const loaded = new Promise<void>((resolve) => {
            const stop = this.session.on("Page.loadEventFired", () => {
                stop();
                resolve();
            });
        });
        await this.session.send("Page.navigate", { url });
        await Promise.race([loaded, delay(8000)]);
        await delay(settle_ms);
    }

    async evaluate<T>(expression: string): Promise<T> {
        const { result } = await this.session.send<
            { result: { value: T } }
        >("Runtime.evaluate", {
            expression,
            awaitPromise: true,
            returnByValue: true,
        });
        return result.value;
    }

    async jpeg(quality = 82): Promise<Uint8Array> {
        const { data } = await this.session.send<{ data: string }>(
            "Page.captureScreenshot",
            { format: "jpeg", quality },
        );
        return Uint8Array.from(atob(data), (c) => c.charCodeAt(0));
    }
}

export function delay(ms: number): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, ms));
}
