import { CommandExitError, Sandbox, SandboxNotFoundError, Template } from "e2b";

import {
    type BuiltImage,
    type Guest,
    ImageMismatch,
    type Owner,
    type Provider,
    requireEnv,
    untilGone,
} from "./provider.ts";

const template_name = "slop-browser";
const abandoned_sandbox_lifetime_ms = 15 * 60_000;

export function e2bRef(input_sha256: string): { ref: string; tag: string } {
    const tag = input_sha256.slice(0, 12);
    return { ref: `${template_name}:${tag}`, tag };
}

async function requireTaggedBuild(image: BuiltImage): Promise<void> {
    const tag = image.details.tag;
    const tags = await Template.getTags(image.id);
    const current = tags.find((entry) => entry.tag === tag)?.buildId ??
        "no build";
    if (current !== image.details.build_id) {
        throw new ImageMismatch(
            "e2b",
            `${image.ref} at build ${image.details.build_id}`,
            current,
        );
    }
}

export function e2bProvider(): Provider {
    requireEnv("E2B_API_KEY", "e2b");
    return {
        name: "e2b",
        sdk: "e2b@2.63.0",
        build_settings: { template_name },
        async build(environment, input_sha256) {
            const { ref, tag } = e2bRef(input_sha256);
            const built = await Template.build(
                Template().fromDockerfile(environment.dockerfile),
                ref,
                {
                    cpuCount: environment.resources.vcpus,
                    memoryMB: environment.resources.memory_mib,
                    onBuildLogs: (entry) => console.error(String(entry)),
                },
            );
            return {
                ref,
                id: built.templateId,
                details: { build_id: built.buildId, tag },
            };
        },
        async start(image: BuiltImage, owner: Owner): Promise<Guest> {
            await requireTaggedBuild(image);
            const sandbox = await Sandbox.create(image.ref, {
                metadata: owner,
                timeoutMs: abandoned_sandbox_lifetime_ms,
                allowInternetAccess: false,
            });
            const info = await sandbox.getInfo();
            if (info.templateId !== image.id) {
                throw new ImageMismatch("e2b", image.id, info.templateId);
            }
            return {
                id: sandbox.sandboxId,
                reported: { vcpus: info.cpuCount, memory_mib: info.memoryMB },
                async write(path, content) {
                    await sandbox.files.write(path, content);
                },
                async exec(command, timeout_ms) {
                    try {
                        const result = await sandbox.commands.run(command, {
                            timeoutMs: timeout_ms,
                        });
                        return {
                            exit_code: result.exitCode,
                            output: `${result.stderr}${result.stdout}`,
                        };
                    } catch (error) {
                        if (!(error instanceof CommandExitError)) throw error;
                        return {
                            exit_code: error.exitCode,
                            output: `${error.stderr}${error.stdout}`,
                        };
                    }
                },
            };
        },
        async owned(owner) {
            const pages = Sandbox.list({ query: { metadata: owner } });
            const ids: string[] = [];
            while (pages.hasNext) {
                for (const info of await pages.nextItems()) {
                    ids.push(info.sandboxId);
                }
            }
            return ids;
        },
        async destroy(id) {
            await Sandbox.kill(id);
            await untilGone(
                id,
                () =>
                    Sandbox.getInfo(id).then(
                        () => false,
                        (error) => {
                            if (error instanceof SandboxNotFoundError) {
                                return true;
                            }
                            throw error;
                        },
                    ),
            );
        },
    };
}
