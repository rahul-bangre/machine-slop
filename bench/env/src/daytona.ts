import { Buffer } from "node:buffer";

import {
    Daytona,
    DaytonaNotFoundError,
    Image,
    SandboxState,
    type Snapshot,
    SnapshotState,
} from "@daytona/sdk";

import {
    type BuiltImage,
    type Guest,
    ImageMismatch,
    type Owner,
    type Provider,
    requireEnv,
    untilGone,
} from "./provider.ts";

const snapshot_prefix = "slop-browser";
const entrypoint = ["sleep", "infinity"];
// A sandbox the smoke run loses track of stops after this many idle minutes
// and, being ephemeral, is deleted when it stops.
const auto_stop_minutes = 15;
const delete_timeout_s = 120;
const activate_timeout_ms = 120_000;
const gone = new Set<string>([SandboxState.DESTROYED]);
const failed = new Set<string>([
    SnapshotState.ERROR,
    SnapshotState.BUILD_FAILED,
]);

export function daytonaSnapshotName(input_sha256: string): string {
    return `${snapshot_prefix}-${input_sha256.slice(0, 12)}`;
}

function builtImage(snapshot: Snapshot): BuiltImage {
    return {
        ref: snapshot.name,
        id: snapshot.id,
        // The registry image the snapshot was built into; `imageName` is empty
        // for a snapshot built from a Dockerfile.
        details: Object.fromEntries(
            Object.entries({
                image_ref: snapshot.ref,
                build_ref: snapshot.buildInfo?.snapshotRef,
            }).filter((entry): entry is [string, string] => !!entry[1]),
        ),
    };
}

export function daytonaProvider(): Provider {
    requireEnv("DAYTONA_API_KEY", "daytona");
    const daytona = new Daytona({ otelEnabled: false });

    async function existing(name: string): Promise<Snapshot | null> {
        try {
            return await daytona.snapshot.get(name);
        } catch (error) {
            if (error instanceof DaytonaNotFoundError) return null;
            throw error;
        }
    }

    // Activation may finish after the request returns; record the snapshot
    // only once it can be used.
    async function activeSnapshot(name: string): Promise<Snapshot> {
        const deadline = Date.now() + activate_timeout_ms;
        while (true) {
            const snapshot = await daytona.snapshot.get(name);
            if (snapshot.state === SnapshotState.ACTIVE) return snapshot;
            if (Date.now() >= deadline) {
                throw new Error(
                    `snapshot ${name} is still ${snapshot.state} after activation`,
                );
            }
            await new Promise((resolve) => setTimeout(resolve, 2_000));
        }
    }

    return {
        name: "daytona",
        sdk: "@daytona/sdk@0.231.0",
        build_settings: { snapshot_prefix, entrypoint },
        async build(environment, input_sha256) {
            const { vcpus, memory_mib } = environment.resources;
            if (memory_mib % 1024 !== 0) {
                throw new Error("Daytona allocates memory in whole GiB");
            }
            // Snapshot names are unique per organization and this one is
            // derived from the inputs, so a snapshot already under it was built
            // from them: reuse it if it built, replace it if it failed.
            const name = daytonaSnapshotName(input_sha256);
            const found = await existing(name);
            if (found?.state === SnapshotState.ACTIVE) return builtImage(found);
            if (found?.state === SnapshotState.INACTIVE) {
                await daytona.snapshot.activate(found);
                return builtImage(await activeSnapshot(name));
            }
            if (found && failed.has(found.state)) {
                console.error(`deleting failed snapshot ${name}`);
                await daytona.snapshot.delete(found);
            } else if (found) {
                throw new Error(
                    `snapshot ${name} is ${found.state}; wait for it and run the build again`,
                );
            }
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
                return builtImage(snapshot);
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
            // The sandbox names its snapshot; a snapshot deleted and rebuilt
            // under the same name has a different ID.
            const snapshot = sandbox.snapshot
                ? await existing(sandbox.snapshot)
                : null;
            if (snapshot?.id !== image.id) {
                throw new ImageMismatch(
                    "daytona",
                    image.id,
                    snapshot?.id ?? "no snapshot",
                );
            }
            return {
                id: sandbox.id,
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
            const lookup = () =>
                daytona.get(id).catch((error) => {
                    if (error instanceof DaytonaNotFoundError) return null;
                    throw error;
                });
            const sandbox = await lookup();
            if (sandbox && !gone.has(sandbox.state ?? "")) {
                await sandbox.delete(delete_timeout_s, true);
            }
            await untilGone(id, async () => {
                const found = await lookup();
                return !found || gone.has(found.state ?? "");
            });
        },
    };
}
