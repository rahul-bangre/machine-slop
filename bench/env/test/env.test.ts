import assert from "node:assert/strict";

import { Image } from "@daytona/sdk";
import { Template } from "e2b";

import chromium from "../../browser/chromium.json" with { type: "json" };
import type { Observation } from "../guest/observe.ts";
import { CredentialInArtifact, serializeArtifact } from "../src/artifacts.ts";
import { effectiveCpus, effectiveMemoryMib, evaluate } from "../src/checks.ts";
import { daytonaSnapshotName } from "../src/daytona.ts";
import { e2bRef } from "../src/e2b.ts";
import { browserEnvironment } from "../src/environment.ts";
import {
    type Guest,
    ImageMismatch,
    type ImageRecord,
    inputSha256,
    type Owner,
    type Provider,
} from "../src/provider.ts";
import { guestFiles, smoke, StaleImage } from "../src/smoke.ts";

const environment = await browserEnvironment();

const container: Observation = {
    deno_version: "2.9.7",
    chromium_version: "Google Chrome for Testing 157.0.8103.50",
    nproc: 2,
    mem_total_kib: 3911568,
    cgroups: [{
        path: "/",
        cpu_max: "200000 100000",
        memory_max: "4294967296",
    }],
    chromium: {
        exit_code: 0,
        timed_out: false,
        dom: '<p id="status">fixture script ran</p><p id="agent">Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) HeadlessChrome/157.0.0.0 Safari/537.36</p>',
        stderr_tail: "",
        fixture_requests: ["GET /", "GET /favicon.ico"],
    },
    leftover_chromium_pids: [],
    profile_removed: true,
};

const microvm: Observation = {
    ...container,
    nproc: 2,
    mem_total_kib: 4_010_000,
    cgroups: [
        { path: "/user.slice", cpu_max: "max 100000", memory_max: "max" },
        { path: "/", cpu_max: null, memory_max: null },
    ],
};

async function recordFor(provider: Provider): Promise<ImageRecord> {
    return {
        provider: provider.name,
        sdk: provider.sdk,
        input_sha256: await inputSha256(provider, environment),
        resources: environment.resources,
        image: { ref: "slop-browser:test", id: "image-1", details: {} },
        built_at: "2026-11-10T00:00:00.000Z",
    };
}

const facts = {
    reported: { vcpus: 2, memory_mib: 4096 },
};

function failed(observation: Observation, guest = facts): string[] {
    return evaluate(environment, guest, observation)
        .filter((check) => !check.ok)
        .map((check) => check.name);
}

Deno.test("the environment pins Chromium, Deno, the base image and 2 vCPU with 4 GiB", async () => {
    const pin = chromium.platforms["linux-x86_64"];
    const deno = (await Deno.readTextFile(".tool-versions")).match(
        /^deno (\S+)$/m,
    )?.[1];
    const { dockerfile } = environment;

    assert.equal(environment.chromium.version, "157.0.8103.50");
    assert.equal(environment.deno.version, deno);
    assert.equal(deno, "2.9.7");
    assert.deepEqual(environment.resources, { vcpus: 2, memory_mib: 4096 });
    assert.match(dockerfile, /^FROM \S+@sha256:[0-9a-f]{64}\n/);
    assert.ok(dockerfile.includes(`${pin.url} && echo '${pin.sha256}  `));
    assert.ok(
        dockerfile.includes(
            `/v2.9.7/deno-x86_64-unknown-linux-gnu.zip && echo 'c6527f24f4b16031d3ae4fa9f658d5f11534c8d84ce7dc8502420280919c3490  `,
        ),
    );
    assert.match(dockerfile, /snapshot\.debian\.org\/archive\/debian\/\d{8}T/);
    assert.doesNotMatch(dockerfile, /:latest\b/);
});

Deno.test("E2B and Daytona read the same build steps from the Dockerfile", async () => {
    const instructions = environment.dockerfile.trim().split("\n");
    const runs = instructions
        .filter((line) => line.startsWith("RUN "))
        .map((line) => line.slice("RUN ".length));

    const e2b = JSON.parse(
        await Template.toJSON(
            Template().fromDockerfile(environment.dockerfile),
            false,
        ),
    );
    assert.equal(e2b.fromImage, instructions[0]!.slice("FROM ".length));
    assert.deepEqual(
        e2b.steps.filter((step: { type: string }) => step.type === "RUN")
            .map((step: { args: string[] }) => step.args[0]),
        runs,
    );

    const context = await Deno.makeTempDir();
    try {
        await Deno.writeTextFile(
            `${context}/Dockerfile`,
            environment.dockerfile,
        );
        const daytona = Image.fromDockerfile(`${context}/Dockerfile`);
        assert.deepEqual(daytona.dockerfile.trim().split("\n"), instructions);
    } finally {
        await Deno.remove(context, { recursive: true });
    }
});

Deno.test("limits are the tightest of what the kernel and every cgroup allow", () => {
    assert.equal(effectiveCpus(8, container.cgroups), 2);
    assert.equal(effectiveCpus(2, microvm.cgroups), 2);
    assert.equal(effectiveCpus(8, microvm.cgroups), 8);
    assert.equal(
        effectiveCpus(8, [
            { path: "/a", cpu_max: "400000 100000", memory_max: null },
            { path: "/", cpu_max: "150000 100000", memory_max: null },
        ]),
        1.5,
    );
    assert.equal(effectiveMemoryMib(16 * 1024 * 1024, container.cgroups), 4096);
    assert.equal(effectiveMemoryMib(3911568, container.cgroups), 3819);
});

Deno.test("a sandbox matching the environment passes, as a container or a microVM", () => {
    assert.deepEqual(failed(container), []);
    assert.deepEqual(failed({ ...container, nproc: 16 }), []);
    assert.deepEqual(failed(microvm), []);
});

Deno.test("each way a sandbox can differ from the environment fails its own check", () => {
    assert.deepEqual(failed({ ...microvm, nproc: 4 }), [
        "guest is limited to the pinned vCPUs",
    ]);
    assert.deepEqual(
        failed({ ...container, nproc: 16, cgroups: microvm.cgroups }),
        ["guest is limited to the pinned vCPUs"],
    );
    assert.deepEqual(failed({ ...microvm, mem_total_kib: 8_000_000 }), [
        "guest is limited to the pinned memory",
    ]);
    assert.deepEqual(failed({ ...microvm, mem_total_kib: 1_000_000 }), [
        "guest is limited to the pinned memory",
    ]);
    assert.deepEqual(
        failed(container, {
            ...facts,
            reported: { vcpus: 2, memory_mib: 1024 },
        }),
        ["provider reports the pinned resources"],
    );
    assert.deepEqual(
        failed({
            ...container,
            nproc: 8,
            cgroups: [{
                path: "/",
                cpu_max: "150000 100000",
                memory_max: null,
            }],
        }),
        ["guest is limited to the pinned vCPUs"],
    );
    assert.deepEqual(failed({ ...container, deno_version: "2.9.6" }), [
        "Deno is the pinned version",
    ]);
    assert.deepEqual(
        failed({
            ...container,
            chromium_version: "Google Chrome for Testing 157.0.8103.49",
        }),
        ["Chromium is the pinned version"],
    );
    assert.deepEqual(
        failed({
            ...container,
            chromium: { ...container.chromium, dom: "script did not run" },
        }),
        ["headless Chromium ran the local fixture"],
    );
    assert.deepEqual(
        failed({
            ...container,
            chromium: { ...container.chromium, fixture_requests: [] },
        }),
        ["headless Chromium ran the local fixture"],
    );
    assert.deepEqual(failed({ ...container, leftover_chromium_pids: [42] }), [
        "no Chromium process outlived the run",
    ]);
    assert.deepEqual(failed({ ...container, profile_removed: false }), [
        "Chromium profile was removed",
    ]);
});

type FakeCloud = {
    sandboxes: Map<string, Owner>;
    created: number;
    destroyed: string[];
    faults: {
        create_response_lost: boolean;
        image_moved: boolean;
        destroy_fails: boolean;
        listing_stale: boolean;
        listing_fails: boolean;
        observer_output: string;
    };
};

function fakeProvider(): Provider & { cloud: FakeCloud } {
    const cloud: FakeCloud = {
        sandboxes: new Map(),
        created: 0,
        destroyed: [],
        faults: {
            create_response_lost: false,
            image_moved: false,
            destroy_fails: false,
            listing_stale: false,
            listing_fails: false,
            observer_output: JSON.stringify(container),
        },
    };
    const matches = (labels: Owner, owner: Owner) =>
        Object.entries(owner).every(([key, value]) => labels[key] === value);
    return {
        cloud,
        name: "fake",
        sdk: "fake@0",
        build_settings: {},
        build: () => Promise.reject(new Error("not used")),
        start(image, owner): Promise<Guest> {
            const id = `sandbox-${++cloud.created}`;
            cloud.sandboxes.set(id, owner);
            if (cloud.faults.create_response_lost) {
                return Promise.reject(new Error("socket hang up"));
            }
            if (cloud.faults.image_moved) {
                return Promise.reject(
                    new ImageMismatch("fake", image.id, "image-2"),
                );
            }
            return Promise.resolve({
                id,
                reported: { vcpus: 2, memory_mib: 4096 },
                write: () => Promise.resolve(),
                exec: () =>
                    Promise.resolve({
                        exit_code: 0,
                        output:
                            `Download chromium...\n${cloud.faults.observer_output}\n`,
                    }),
            });
        },
        owned(owner) {
            if (cloud.faults.listing_fails) {
                return Promise.reject(new Error("bad gateway"));
            }
            if (cloud.faults.listing_stale) {
                return Promise.resolve([...cloud.destroyed]);
            }
            return Promise.resolve(
                [...cloud.sandboxes]
                    .filter(([, labels]) => matches(labels, owner))
                    .map(([id]) => id),
            );
        },
        destroy(id) {
            if (cloud.faults.destroy_fails) {
                return Promise.reject(new Error("service unavailable"));
            }
            if (cloud.sandboxes.delete(id)) cloud.destroyed.push(id);
            return Promise.resolve();
        },
    };
}

const files = await guestFiles();

Deno.test("a smoke run observes the guest, passes, and destroys only its own sandbox", async () => {
    const provider = fakeProvider();
    provider.cloud.sandboxes.set("someone-else", { "env-smoke-run": "other" });

    const report = await smoke(
        provider,
        environment,
        await recordFor(provider),
        files,
        "run-1",
    );

    assert.equal(report.failure, null);
    assert.ok(report.passed, JSON.stringify(report.checks, null, 2));
    assert.deepEqual(report.release, {
        destroyed: ["sandbox-1"],
        unconfirmed: [],
        listing_error: null,
    });
    assert.deepEqual([...provider.cloud.sandboxes.keys()], ["someone-else"]);
});

Deno.test("a sandbox whose create response was lost is still destroyed, and the run fails", async () => {
    const provider = fakeProvider();
    provider.cloud.faults.create_response_lost = true;

    const report = await smoke(
        provider,
        environment,
        await recordFor(provider),
        files,
        "run-2",
    );

    assert.equal(report.passed, false);
    assert.equal(report.failure, "socket hang up");
    assert.deepEqual(provider.cloud.destroyed, ["sandbox-1"]);
    assert.deepEqual(report.release, {
        destroyed: ["sandbox-1"],
        unconfirmed: [],
        listing_error: null,
    });
});

Deno.test("a run that cannot destroy its sandbox fails even when every check passed", async () => {
    const provider = fakeProvider();
    provider.cloud.faults.destroy_fails = true;

    const report = await smoke(
        provider,
        environment,
        await recordFor(provider),
        files,
        "run-3",
    );

    assert.equal(report.passed, false);
    assert.deepEqual(report.release, {
        destroyed: [],
        unconfirmed: ["sandbox-1"],
        listing_error: null,
    });
    assert.deepEqual(
        report.checks.filter((check) => !check.ok).map((check) => check.name),
        ["every sandbox this run created was destroyed"],
    );
});

Deno.test("an observer that does not report fails the run instead of being judged", async () => {
    const provider = fakeProvider();
    provider.cloud.faults.observer_output = "error: Module not found";

    const report = await smoke(
        provider,
        environment,
        await recordFor(provider),
        files,
        "run-4",
    );

    assert.equal(report.passed, false);
    assert.match(report.failure ?? "", /observer exited 0/);
    assert.equal(report.observation, null);
    assert.deepEqual(provider.cloud.sandboxes.size, 0);
});

Deno.test("an image built from other inputs is refused before any sandbox is created", async () => {
    const provider = fakeProvider();
    const record = await recordFor(provider);

    await assert.rejects(
        smoke(
            provider,
            { ...environment, resources: { vcpus: 2, memory_mib: 2048 } },
            record,
            files,
            "run-5",
        ),
        StaleImage,
    );
    await assert.rejects(
        smoke(
            provider,
            environment,
            { ...record, input_sha256: "0".repeat(64) },
            files,
            "run-5",
        ),
        StaleImage,
    );
    assert.equal(provider.cloud.created, 0);
});

Deno.test("an artifact that would contain a credential is refused", () => {
    const environment = { DAYTONA_API_KEY: "dtn_0123456789abcdef" };
    const report = {
        failure: "request failed: Authorization: Bearer dtn_0123456789abcdef",
    };

    assert.throws(
        () => serializeArtifact("report.json", report, environment),
        CredentialInArtifact,
    );
    assert.equal(
        serializeArtifact("report.json", { failure: "timed out" }, environment),
        '{\n  "failure": "timed out"\n}\n',
    );
    assert.doesNotThrow(() =>
        serializeArtifact("report.json", report, { DAYTONA_API_KEY: "" })
    );
});

Deno.test("a sandbox that would not run the recorded build fails the run and is destroyed", async () => {
    const provider = fakeProvider();
    provider.cloud.faults.image_moved = true;

    const report = await smoke(
        provider,
        environment,
        await recordFor(provider),
        files,
        "run-6",
    );

    assert.equal(report.passed, false);
    assert.match(report.failure ?? "", /would not run the recorded image/);
    assert.equal(report.observation, null);
    assert.deepEqual(provider.cloud.destroyed, ["sandbox-1"]);
});

Deno.test("images are named by their input digest, once", () => {
    const digest = "9853e4b677dd" + "0".repeat(52);
    assert.deepEqual(e2bRef(digest), {
        ref: "slop-browser:9853e4b677dd",
        tag: "9853e4b677dd",
    });
    assert.equal(daytonaSnapshotName(digest), "slop-browser-9853e4b677dd");
});

Deno.test("the sandbox a run started is destroyed even when the listing is stale", async () => {
    const provider = fakeProvider();
    provider.cloud.faults.listing_stale = true;
    provider.cloud.destroyed.push("sandbox-0");

    const report = await smoke(
        provider,
        environment,
        await recordFor(provider),
        files,
        "run-7",
    );
    assert.ok(report.passed, JSON.stringify(report.checks, null, 2));
    assert.deepEqual(provider.cloud.destroyed, ["sandbox-0", "sandbox-1"]);
    assert.equal(provider.cloud.sandboxes.size, 0);
});

Deno.test("a failed listing still destroys the sandbox the run started, and fails the run", async () => {
    const provider = fakeProvider();
    provider.cloud.faults.listing_fails = true;

    const report = await smoke(
        provider,
        environment,
        await recordFor(provider),
        files,
        "run-8",
    );

    assert.equal(report.passed, false);
    assert.deepEqual(report.release, {
        destroyed: ["sandbox-1"],
        unconfirmed: [],
        listing_error: "bad gateway",
    });
    assert.equal(provider.cloud.sandboxes.size, 0);
});
