import { type Environment, type Resources, sha256 } from "./environment.ts";

// What a provider built: `ref` is what a sandbox is created from, `id` is the
// provider's identifier for the built image, and `details` holds whatever else
// the provider needs to prove a sandbox came from this build.
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

export interface Provider {
    readonly name: string;
    readonly sdk: string;
    // Everything the provider's build takes besides the Dockerfile and the
    // resources, so the input digest covers the whole build.
    readonly build_settings: Record<string, unknown>;
    // Builds the image, or returns the one an earlier build with the same
    // inputs already produced.
    build(environment: Environment, input_sha256: string): Promise<BuiltImage>;
    // Creates a sandbox labelled with `owner` from exactly this build, or
    // throws ImageMismatch. If it throws, a sandbox may still exist; `owned`
    // finds it.
    start(image: BuiltImage, owner: Owner): Promise<Guest>;
    owned(owner: Owner): Promise<string[]>;
    // Resolves only once a lookup by ID shows the sandbox is gone, not when
    // deletion is accepted.
    destroy(id: string): Promise<void>;
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
    provider: Provider,
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
