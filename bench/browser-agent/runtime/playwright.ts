import { chromium, type Locator, type Page } from "playwright-core";
import type {
    Action,
    Condition,
    Driver,
    State,
    Target,
    View,
} from "./driver.ts";

class PlaywrightDriver implements Driver {
    private epoch = 0;
    private readonly refs = new Map<string, string>();

    constructor(
        private readonly page: Page,
        private readonly detach: () => Promise<void>,
    ) {
        page.on("framenavigated", (frame) => {
            if (frame !== page.mainFrame()) return;
            this.epoch++;
            this.refs.clear();
        });
    }

    private target(target: Target): Locator {
        if ("ref" in target) {
            const ref = this.refs.get(target.ref);
            if (!ref) {
                throw new Error(`unknown or stale reference: ${target.ref}`);
            }
            return this.page.getByRef(ref);
        }
        const root = target.scope
            ? this.target({ ref: target.scope })
            : this.page;
        return root.getByRole(
            target.role as Parameters<Page["getByRole"]>[0],
            { name: target.name, exact: true },
        );
    }

    async observe(view: View): Promise<State> {
        const epoch = this.epoch;
        const scope = view.mode === "scope"
            ? this.target(view.target)
            : undefined;
        const ancestor = view.mode === "scope" ? view.parent ?? 0 : 0;
        if (!Number.isInteger(ancestor) || ancestor < 0 || ancestor > 4) {
            throw new Error("parent must be an integer from 0 to 4");
        }
        const subtree = scope && ancestor > 0
            ? scope.locator(`xpath=${"../".repeat(ancestor).slice(0, -1)}`)
            : scope;
        const modal = view.mode === "overview"
            ? this.page.locator(
                'dialog:modal:visible, [role="dialog"][aria-modal="true"]:visible, [role="alertdialog"][aria-modal="true"]:visible',
            ).last()
            : undefined;
        const root = subtree ??
            (modal && await modal.count() ? modal : this.page);
        const bounded_overview = view.mode === "overview" && root === this.page;
        const snapshot = await root.ariaSnapshot({
            mode: "ai",
            timeout: 5_000,
            ...(bounded_overview ? { depth: 6 } : {}),
        });
        if (epoch !== this.epoch) {
            throw new Error(
                "document navigated during observation; observe again",
            );
        }
        const lines = snapshot.split("\n");
        const bounded = bounded_overview ? lines.slice(0, 180) : lines;
        const text = bounded.join("\n").replace(
            /\[ref=([^\]]+)\]/g,
            (match, ref: string) => {
                const issued = `${epoch}:${ref}`;
                this.refs.set(issued, ref);
                return match.replace(ref, issued);
            },
        );
        return {
            text: bounded_overview
                ? `${text}\n[Overview limited to depth 6 and 180 lines; deeper or remaining content may be omitted. Request full or scope for omitted content.]`
                : text,
            scope: subtree
                ? "requested subtree"
                : root === this.page
                ? "document"
                : "active modal dialog",
            format: "playwright-aria-ai-yaml",
        };
    }

    async act(action: Action, timeout_ms: number): Promise<void> {
        switch (action.kind) {
            case "click":
                await this.target(action.target).click({ timeout: timeout_ms });
                return;
            case "fill":
                await this.target(action.target).fill(action.text, {
                    timeout: timeout_ms,
                });
                return;
            case "press":
                await this.page.keyboard.press(action.key);
                return;
            case "scroll":
                await this.page.mouse.wheel(0, action.dy);
                return;
        }
    }

    async wait(condition: Condition, timeout_ms: number): Promise<void> {
        if ("text" in condition) {
            await this.page.getByText(condition.text, { exact: false }).first()
                .waitFor({ state: "visible", timeout: timeout_ms });
            return;
        }
        const target = this.target(condition.target);
        const deadline = performance.now() + timeout_ms;
        while (true) {
            const remaining = Math.max(1, deadline - performance.now());
            const satisfied = "value" in condition
                ? await target.inputValue({ timeout: remaining }) ===
                    condition.value
                : await target.isChecked({ timeout: remaining }) ===
                    condition.checked;
            if (satisfied) return;
            if (performance.now() >= deadline) {
                throw new Error("condition timed out");
            }
            await new Promise((resolve) => setTimeout(resolve, 25));
        }
    }

    close(): Promise<void> {
        return this.detach();
    }
}

export async function connectPlaywright(endpoint: string): Promise<Driver> {
    const browser = await chromium.connectOverCDP(endpoint);
    const page = browser.contexts()[0]?.pages()[0];
    if (!page) {
        await browser.close();
        throw new Error("owned Chrome has no page");
    }
    return new PlaywrightDriver(page, () => browser.close());
}
