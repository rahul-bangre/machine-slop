import { Host, HttpError } from "@machine-slop/sdk";

import { shellWithin } from "../../agent/src/agentd-sandbox.ts";
import type { Resources } from "./environment.ts";
import { type Guest, ProviderNotReady, type Sandboxes } from "./provider.ts";

const request_timeout_ms = 30_000;
const boot_timeout_ms = 60_000;
const abandoned_machine_idle_secs = 15 * 60;

export async function machinedSandboxes(
    url: string,
    resources: Resources,
): Promise<Sandboxes> {
    const host = new Host(url);
    const running = await host.machines(AbortSignal.timeout(request_timeout_ms))
        .catch(() => {
            throw new ProviderNotReady(`machined at ${url} is not reachable`);
        });
    if (running.length > 0) {
        throw new ProviderNotReady(
            `machined at ${url} already runs ${running.length} machines. Machines carry no owner, so the smoke test needs the host to itself and destroys every machine it finds.`,
        );
    }
    return {
        name: "machined",
        async start(): Promise<Guest> {
            const { machine, info } = await host.createMachine({
                vcpus: resources.vcpus,
                memory_mib: resources.memory_mib,
                idle_timeout_secs: abandoned_machine_idle_secs,
                allowed_hosts: [],
            }, AbortSignal.timeout(boot_timeout_ms));
            return {
                id: info.id,
                reported: { vcpus: info.vcpus, memory_mib: info.memory_mib },
                write: (path, content) =>
                    machine.writeFile(path, content, {
                        signal: AbortSignal.timeout(request_timeout_ms),
                    }),
                async exec(command, timeout_ms) {
                    const result = await shellWithin(
                        timeout_ms,
                        machine,
                        command,
                    );
                    if (result.timed_out) {
                        throw new Error(
                            `${command} did not finish within ${timeout_ms}ms`,
                        );
                    }
                    if (result.exit_code === null) {
                        throw new Error(`${command} was killed by a signal`);
                    }
                    return {
                        exit_code: result.exit_code,
                        output: `${result.stderr}${result.stdout}`,
                    };
                },
            };
        },
        async owned() {
            const machines = await host.machines(
                AbortSignal.timeout(request_timeout_ms),
            );
            return machines.map((machine) => machine.id);
        },
        async destroy(id) {
            await host.machine(id).destroy(
                AbortSignal.timeout(request_timeout_ms),
            ).catch((error) => {
                if (!(error instanceof HttpError && error.status === 404)) {
                    throw error;
                }
            });
        },
    };
}
