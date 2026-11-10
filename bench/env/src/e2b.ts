import { CommandExitError, Sandbox, Template } from "e2b";

import {
    type BuiltImage,
    type Guest,
    type Owner,
    type Provider,
    requireEnv,
} from "./provider.ts";

const template_name = "slop-browser";
// A sandbox the smoke run loses track of is killed by E2B after this long.
const sandbox_lifetime_ms = 15 * 60_000;

export function e2bProvider(): Provider {
    requireEnv("E2B_API_KEY", "e2b");
    return {
        name: "e2b",
        sdk: "e2b@2.63.0",
        build_settings: { template_name },
        async build(environment, input_sha256) {
            // The tag names this exact build, so later builds of the template
            // cannot change what the record points at.
            const tag = input_sha256.slice(0, 12);
            const built = await Template.build(
                Template().fromDockerfile(environment.dockerfile),
                `${template_name}:${tag}`,
                {
                    cpuCount: environment.resources.vcpus,
                    memoryMB: environment.resources.memory_mib,
                    onBuildLogs: (entry) => console.error(String(entry)),
                },
            );
            return {
                ref: `${built.name}:${tag}`,
                id: built.templateId,
                details: { build_id: built.buildId, tag },
            };
        },
        async start(image: BuiltImage, owner: Owner): Promise<Guest> {
            const sandbox = await Sandbox.create(image.ref, {
                metadata: owner,
                timeoutMs: sandbox_lifetime_ms,
                allowInternetAccess: false,
            });
            const info = await sandbox.getInfo();
            return {
                id: sandbox.sandboxId,
                image_id: info.templateId,
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
        },
    };
}
