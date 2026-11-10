import { Buffer } from "node:buffer";

import { Daytona, Image, SandboxState, SnapshotState } from "@daytona/sdk";

import {
    type BuiltImage,
    type Guest,
    type Owner,
    type Provider,
    requireEnv,
} from "./provider.ts";

const snapshot_prefix = "slop-browser";
const entrypoint = ["sleep", "infinity"];
// A sandbox the smoke run loses track of stops after this many idle minutes
// and, being ephemeral, is deleted when it stops.
const auto_stop_minutes = 15;
const gone = new Set<string>([SandboxState.DESTROYED, SandboxState.DESTROYING]);

export function daytonaProvider(): Provider {
    requireEnv("DAYTONA_API_KEY", "daytona");
    const daytona = new Daytona({ otelEnabled: false });
    return {
        name: "daytona",
        sdk: "@daytona/sdk@0.231.0",
        build_settings: { snapshot_prefix, entrypoint },
        async build(environment, input_sha256) {
            const { vcpus, memory_mib } = environment.resources;
            if (memory_mib % 1024 !== 0) {
                throw new Error("Daytona allocates memory in whole GiB");
            }
            // Snapshot names are unique, so a name that already exists is an
            // error rather than something this build silently reuses.
            const name = `${snapshot_prefix}-${input_sha256.slice(0, 12)}`;
            const context = await Deno.makeTempDir({ prefix: "env-build-" });
            try {
                const dockerfile = `${context}/Dockerfile`;
                await Deno.writeTextFile(dockerfile, environment.dockerfile);
                const snapshot = await daytona.snapshot.create({
                    name,
                    image: Image.fromDockerfile(dockerfile),
                    resources: { cpu: vcpus, memory: memory_mib / 1024 },
                    entrypoint,
                }, { onLogs: (chunk) => console.error(chunk.trimEnd()) });
                if (snapshot.state !== SnapshotState.ACTIVE) {
                    throw new Error(
                        `snapshot ${name} is ${snapshot.state}, not active`,
                    );
                }
                return {
                    ref: snapshot.name,
                    id: snapshot.id,
                    details: { image_name: snapshot.imageName ?? "" },
                };
            } finally {
                await Deno.remove(context, { recursive: true });
            }
        },
        async start(image: BuiltImage, owner: Owner): Promise<Guest> {
            const sandbox = await daytona.create({
                snapshot: image.ref,
                labels: owner,
                ephemeral: true,
                autoStopInterval: auto_stop_minutes,
                networkBlockAll: true,
            });
            // The sandbox names its snapshot; resolve that name to the ID the
            // record holds, so a snapshot rebuilt under the same name is caught.
            const snapshot = sandbox.snapshot
                ? await daytona.snapshot.get(sandbox.snapshot)
                : null;
            return {
                id: sandbox.id,
                image_id: snapshot?.id ?? "",
                reported: {
                    vcpus: sandbox.cpu,
                    memory_mib: sandbox.memory * 1024,
                },
                async write(path, content) {
                    await sandbox.fs.uploadFile(Buffer.from(content), path);
                },
                async exec(command, timeout_ms) {
                    const result = await sandbox.process.executeCommand(
                        command,
                        undefined,
                        undefined,
                        Math.ceil(timeout_ms / 1000),
                    );
                    return {
                        exit_code: result.exitCode,
                        output: result.result,
                    };
                },
            };
        },
        async owned(owner) {
            const ids: string[] = [];
            for await (const sandbox of daytona.list({ labels: owner })) {
                if (!gone.has(sandbox.state ?? "")) ids.push(sandbox.id);
            }
            return ids;
        },
        async destroy(id) {
            await (await daytona.get(id)).delete();
        },
    };
}
