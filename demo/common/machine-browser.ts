import type { HostedMachine } from "../../packages/sdk/src/mod.ts";
import { Browser, delay, type Page } from "./browser.ts";

export type PageText = {
    title: string;
    url: string;
    text: string;
    links: string[];
    height: number;
};

export async function startChromium(machine: HostedMachine): Promise<void> {
    await machine.startProcess({
        program: "/usr/local/bin/chromium",
        args: [
            "--headless=new",
            "--no-sandbox",
            "--hide-scrollbars",
            "--remote-debugging-port=9222",
            "--remote-allow-origins=*",
            "--renderer-process-limit=2",
            "--disable-dev-shm-usage",
            "--user-data-dir=/tmp/chrome",
            "about:blank",
        ],
        cwd: "/workspace",
    });
}

export async function connectChromium(
    machine: HostedMachine,
): Promise<Browser> {
    const forward = machine.forward(9222);
    const base = forward.url.replace(/\/$/, "");
    for (let attempt = 0; attempt < 80; attempt++) {
        const version = await fetch(`${base}/json/version`).then((r) =>
            r.ok ? r.json() : null
        ).catch(() => null);
        if (version?.webSocketDebuggerUrl) {
            const websocket = String(version.webSocketDebuggerUrl).replace(
                /^ws:\/\/[^/]+/,
                base.replace(/^http/, "ws"),
            );
            return await Browser.connect(websocket, () => forward.close());
        }
        await delay(250);
    }
    await forward.close();
    throw new Error("chromium in the machine did not answer");
}

export function readPage(page: Page): Promise<PageText> {
    return page.evaluate<PageText>(`(() => {
        const root = document.querySelector("article, .markdown-body, main") ?? document.body;
        const links = [...new Set([...root.querySelectorAll("a[href]")]
            .map((a) => (a.innerText.trim().replace(/\\s+/g, " ").slice(0, 80) || "link") + " -> " + a.href)
            .filter((line) => line.includes("http")))].slice(0, 60);
        return {
            title: document.title,
            url: location.href,
            text: root.innerText.replace(/\\n{3,}/g, "\\n\\n").slice(0, 14000),
            links,
            height: document.documentElement.scrollHeight,
        };
    })()`);
}
