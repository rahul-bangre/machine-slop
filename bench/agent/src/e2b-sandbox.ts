import { CommandExitError, Sandbox as E2b, TimeoutError } from "e2b";

import {
    type CommandResult,
    OpenSandboxes,
    ProviderNotReady,
    type Sandbox,
    type SandboxProvider,
} from "./sandbox.ts";

const work_directory = "/home/user/task";
const sandbox_lifetime_ms = 30 * 60_000;

class E2bSandbox implements Sandbox {
    readonly reported_boot_ms = null;

    constructor(private readonly remote: E2b) {}

    async writeFile(relative_path: string, content: string): Promise<void> {
        await this.remote.files.write(
            `${work_directory}/${relative_path}`,
            content,
        );
    }

    async run(script: string, timeout_ms: number): Promise<CommandResult> {
        try {
            const result = await this.remote.commands.run(script, {
                cwd: work_directory,
                timeoutMs: timeout_ms,
            });
            return {
                exit_code: result.exitCode,
                stdout: result.stdout,
                stderr: result.stderr,
                timed_out: false,
            };
        } catch (error) {
            if (error instanceof CommandExitError) {
                return {
                    exit_code: error.exitCode,
                    stdout: error.stdout,
                    stderr: error.stderr,
                    timed_out: false,
                };
            }
            if (error instanceof TimeoutError) {
                return {
                    exit_code: null,
                    stdout: "",
                    stderr: "",
                    timed_out: true,
                };
            }
            throw error;
        }
    }

    async close(): Promise<void> {
        await this.remote.kill();
    }
}

export function e2bProvider(template: string): SandboxProvider {
    const open = new OpenSandboxes();
    return {
        name: "e2b",
        describe: () => ({
            template,
            isolation: "E2B cloud sandbox",
            create_measures: "starting a new sandbox from the template",
        }),
        prepare() {
            if (!Deno.env.get("E2B_API_KEY")) {
                throw new ProviderNotReady("the e2b sandbox needs E2B_API_KEY");
            }
            return Promise.resolve();
        },
        reclaim: () => open.closeAll(),
        async create() {
            const remote = await E2b.create(template, {
                timeoutMs: sandbox_lifetime_ms,
            });
            try {
                await remote.commands.run(`mkdir -p ${work_directory}`);
                return open.track(new E2bSandbox(remote));
            } catch (error) {
                await remote.kill();
                throw error;
            }
        },
    };
}
