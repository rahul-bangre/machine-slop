import { encode } from "gpt-tokenizer/encoding/o200k_base";

import { type AxNode, renderAxTree } from "./ax.ts";
import { viewport } from "./browser.ts";
import type { Page } from "./page.ts";

export type Observation = {
    extension: string;
    bytes: Uint8Array<ArrayBuffer>;
    text: string | null;
    tokens: number;
};

export type Observer = {
    format: string;
    tokenizer: string;
    prepare?(page: Page): Promise<void>;
    observe(page: Page): Promise<Observation>;
};

const text_tokenizer = "o200k_base (gpt-tokenizer 4.1.0)";
const image_tokenizer = "estimate: ceil(width * height / 750)";

function textObservation(text: string): Observation {
    return {
        extension: "txt",
        bytes: new TextEncoder().encode(text),
        text,
        tokens: encode(text).length,
    };
}

export const observers: Record<string, Observer> = {
    "ax-tree": {
        format: "ax-tree/indented-v1 (Accessibility.getFullAXTree)",
        tokenizer: text_tokenizer,
        async prepare(page) {
            await page.session.send("Accessibility.enable");
        },
        async observe(page) {
            const { nodes } = await page.session.send<{ nodes: AxNode[] }>(
                "Accessibility.getFullAXTree",
            );
            return textObservation(renderAxTree(nodes));
        },
    },
    html: {
        format: "dom/outer-html",
        tokenizer: text_tokenizer,
        async observe(page) {
            return textObservation(
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
