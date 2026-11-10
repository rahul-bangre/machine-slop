import { viewport } from "./browser.ts";
import { type CdpSession, TimeoutError } from "./cdp.ts";

export type Action =
    | { goto: string }
    | { click: string }
    | { fill: string; text: string }
    | { press: "Enter" };

export class ActionError extends Error {
    constructor(message: string) {
        super(message);
        this.name = "ActionError";
    }
}

export const readiness = { poll_ms: 20, quiet_ms: 250 };

const keys = {
    Enter: {
        key: "Enter",
        code: "Enter",
        windowsVirtualKeyCode: 13,
        text: "\r",
    },
};

export class Page {
    private readonly inflight = new Map<string, string>();

    private constructor(readonly session: CdpSession) {
        session.on("Network.requestWillBeSent", (params) => {
            const request = params.request as { url: string };
            this.inflight.set(String(params.requestId), request.url);
        });
        for (const settled of ["loadingFinished", "loadingFailed"]) {
            session.on(`Network.${settled}`, (params) => {
                this.inflight.delete(String(params.requestId));
            });
        }
    }

    static async prepare(session: CdpSession): Promise<Page> {
        const page = new Page(session);
        await session.send("Page.enable");
        await session.send("Network.enable");
        await session.send("Emulation.setDeviceMetricsOverride", {
            ...viewport,
            mobile: false,
        });
        return page;
    }

    async evaluate<T>(expression: string): Promise<T> {
        const { result, exceptionDetails } = await this.session.send<{
            result: { value: T };
            exceptionDetails?: {
                text: string;
                exception?: { description?: string };
            };
        }>("Runtime.evaluate", {
            expression,
            returnByValue: true,
            awaitPromise: true,
        });
        if (exceptionDetails) {
            throw new ActionError(
                exceptionDetails.exception?.description ??
                    exceptionDetails.text,
            );
        }
        return result.value;
    }

    async act(action: Action): Promise<void> {
        if ("goto" in action) {
            const { errorText } = await this.session.send<
                { errorText?: string }
            >(
                "Page.navigate",
                { url: action.goto },
            );
            if (errorText) {
                throw new ActionError(`navigation failed: ${errorText}`);
            }
        } else if ("click" in action) {
            await this.click(action.click);
        } else if ("fill" in action) {
            await this.click(action.fill);
            await this.session.send("Input.insertText", { text: action.text });
        } else {
            const key = keys[action.press];
            await this.session.send("Input.dispatchKeyEvent", {
                type: "keyDown",
                ...key,
            });
            await this.session.send("Input.dispatchKeyEvent", {
                type: "keyUp",
                ...key,
            });
        }
    }

    async settle(predicate: string, timeout_ms: number): Promise<number> {
        const deadline = performance.now() + timeout_ms;
        const check =
            `document.readyState === "complete" && Boolean(${predicate})`;
        let settled_at: number | null = null;
        while (performance.now() < deadline) {
            const now = performance.now();
            const holds = this.inflight.size === 0 &&
                await this.evaluate<boolean>(check).catch(() => false);
            if (!holds) {
                settled_at = null;
            } else {
                settled_at ??= now;
                if (now - settled_at >= readiness.quiet_ms) return settled_at;
            }
            await new Promise((resolve) =>
                setTimeout(resolve, readiness.poll_ms)
            );
        }
        const pending = [...this.inflight.values()].slice(0, 3).join(", ");
        throw new TimeoutError(
            `readiness (${predicate}; ${this.inflight.size} requests in flight: ${pending})`,
            timeout_ms,
        );
    }

    private async click(selector: string): Promise<void> {
        const point = await this.evaluate<
            { x: number; y: number } | { obscured_by: string } | null
        >(`(() => {
            const element = document.querySelector(${JSON.stringify(selector)});
            if (!element) return null;
            element.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
            const box = element.getBoundingClientRect();
            if (box.width === 0 || box.height === 0) return null;
            const x = box.x + box.width / 2;
            const y = box.y + box.height / 2;
            const hit = document.elementFromPoint(x, y);
            if (!hit || !(element.contains(hit) || hit.contains(element))) {
                return { obscured_by: hit ? hit.outerHTML.slice(0, 120) : "nothing" };
            }
            return { x, y };
        })()`);
        if (!point) {
            throw new ActionError(`no clickable element for ${selector}`);
        }
        if ("obscured_by" in point) {
            throw new ActionError(
                `${selector} is obscured by ${point.obscured_by}`,
            );
        }
        const mouse = { ...point, button: "left", clickCount: 1 };
        await this.session.send("Input.dispatchMouseEvent", {
            type: "mouseMoved",
            ...point,
        });
        await this.session.send("Input.dispatchMouseEvent", {
            type: "mousePressed",
            ...mouse,
        });
        await this.session.send("Input.dispatchMouseEvent", {
            type: "mouseReleased",
            ...mouse,
        });
    }
}
