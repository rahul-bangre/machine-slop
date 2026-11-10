import { type Environment, type Resources, sha256 } from "./environment.ts";

// What a provider built: `ref` is what a sandbox is created from, `id` is the
// provider's identifier for the built image, which a running sandbox reports.
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
    readonly image_id: string;
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
    build(environment: Environment, input_sha256: string): Promise<BuiltImage>;
    // Creates a sandbox labelled with `owner`. If it throws, a sandbox may still
    // exist; `owned` finds it.
    start(image: BuiltImage, owner: Owner): Promise<Guest>;
    owned(owner: Owner): Promise<string[]>;
    destroy(id: string): Promise<void>;
}

export class ProviderNotReady extends Error {
    constructor(message: string) {
        super(message);
        this.name = "ProviderNotReady";
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
