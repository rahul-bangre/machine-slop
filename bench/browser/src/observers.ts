import { encode } from "gpt-tokenizer/encoding/o200k_base";

import { type AxNode, renderAxTree } from "./ax.ts";
import { viewport } from "./browser.ts";
import { applyDelta, diffLines } from "./delta.ts";
import type { Page } from "./page.ts";

export type Observation = {
    delivery: "full" | "delta";
    extension: string;
    bytes: Uint8Array<ArrayBuffer>;
    text: string | null;
    tokens: number;
};

export type Observer = {
    format: string;
    tokenizer: string;
    rebuild?(previous_state: string, delta: string): string;
    prepare?(page: Page): Promise<void>;
    observe(page: Page, previous_state: string | null): Promise<Observation>;
};

const text_tokenizer = "o200k_base (gpt-tokenizer 4.1.0)";
const image_tokenizer = "estimate: ceil(width * height / 750)";

function textObservation(
    delivery: Observation["delivery"],
    text: string,
): Observation & { text: string } {
    return {
        delivery,
        extension: "txt",
        bytes: new TextEncoder().encode(text),
        text,
        tokens: encode(text).length,
    };
}

async function enableAccessibility(page: Page): Promise<void> {
    await page.session.send("Accessibility.enable");
}

async function fullAxText(page: Page): Promise<string> {
    const { nodes } = await page.session.send<{ nodes: AxNode[] }>(
        "Accessibility.getFullAXTree",
    );
    return renderAxTree(nodes);
}

export const observers: Record<string, Observer> = {
    "ax-tree": {
        format: "ax-tree/indented-v1 (Accessibility.getFullAXTree)",
        tokenizer: text_tokenizer,
        prepare: enableAccessibility,
        async observe(page) {
            return textObservation("full", await fullAxText(page));
        },
    },
    "ax-diff": {
        format:
            "ax-tree/indented-v1, or line-delta-v1 against the previous state when that is fewer tokens; diffed from a full Accessibility.getFullAXTree capture",
        tokenizer: text_tokenizer,
        prepare: enableAccessibility,
        rebuild: applyDelta,
        async observe(page, previous_state) {
            const full = textObservation("full", await fullAxText(page));
            if (previous_state === null) return full;
            const delta = textObservation(
                "delta",
                diffLines(previous_state, full.text),
            );
            return delta.tokens < full.tokens ? delta : full;
        },
    },
    html: {
        format: "dom/outer-html",
        tokenizer: text_tokenizer,
        async observe(page) {
            return textObservation(
                "full",
                await page.evaluate<string>(
                    "document.documentElement.outerHTML",
                ),
            );
        },
    },
    screenshot: {
        format: `image/png viewport ${viewport.width}x${viewport.height}`,
        tokenizer: image_tokenizer,
        async observe(page) {
            const { data } = await page.session.send<{ data: string }>(
                "Page.captureScreenshot",
                { format: "png" },
            );
            return {
                delivery: "full",
                extension: "png",
                bytes: Uint8Array.from(
                    atob(data),
                    (char) => char.charCodeAt(0),
                ),
                text: null,
                tokens: Math.ceil(viewport.width * viewport.height / 750),
            };
        },
    },
};
