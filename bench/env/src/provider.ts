import { type Environment, type Resources, sha256 } from "./environment.ts";

export type BuiltImage = {
    ref: string;
    id: string;
    details: Record<string, string>;
};

export type ImageRecord = {
    provider: string;
    sdk: string;
    input_sha256: string;
    resources: Resources;
    image: BuiltImage;
    built_at: string;
};

export type Exec = { exit_code: number; output: string };

export interface Guest {
    readonly id: string;
    readonly reported: Resources;
    write(path: string, content: string): Promise<void>;
    exec(command: string, timeout_ms: number): Promise<Exec>;
}

export type Owner = Record<string, string>;

export interface Sandboxes {
    readonly name: string;
    start(owner: Owner): Promise<Guest>;
    owned(owner: Owner): Promise<string[]>;
    destroy(id: string): Promise<void>;
}

export interface ImageBuilder {
    readonly name: string;
    readonly sdk: string;
    readonly build_settings: Record<string, unknown>;
    build(environment: Environment, input_sha256: string): Promise<BuiltImage>;
    sandboxes(image: BuiltImage): Sandboxes;
}

const gone_timeout_ms = 60_000;

export async function untilGone(
    id: string,
    isGone: () => Promise<boolean>,
): Promise<void> {
    const deadline = Date.now() + gone_timeout_ms;
    while (!await isGone()) {
        if (Date.now() >= deadline) {
            throw new Error(
                `${id} still exists ${gone_timeout_ms}ms after deletion`,
            );
        }
        await new Promise((resolve) => setTimeout(resolve, 1_000));
    }
}

export class ProviderNotReady extends Error {
    constructor(message: string) {
        super(message);
        this.name = "ProviderNotReady";
    }
}

export class ImageMismatch extends Error {
    constructor(provider: string, expected: string, actual: string) {
        super(
            `${provider} would not run the recorded image: expected ${expected}, found ${actual}`,
        );
        this.name = "ImageMismatch";
    }
}

export function inputSha256(
    provider: ImageBuilder,
    environment: Environment,
): Promise<string> {
    return sha256(JSON.stringify({
        provider: provider.name,
        dockerfile: environment.dockerfile,
        resources: environment.resources,
        build_settings: provider.build_settings,
    }));
}

export function requireEnv(name: string, provider: string): void {
    if (!Deno.env.get(name)) {
        throw new ProviderNotReady(`the ${provider} provider needs ${name}`);
    }
}
