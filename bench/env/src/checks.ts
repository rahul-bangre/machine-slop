import type { CgroupLimits, Observation } from "../guest/observe.ts";
import type { Environment, Resources } from "./environment.ts";

export type Check = {
    name: string;
    ok: boolean;
    expected: string;
    observed: string;
};

export type GuestFacts = { reported: Resources };

const min_memory_fraction = 0.9;

export function effectiveCpus(nproc: number, cgroups: CgroupLimits[]): number {
    let cpus = nproc;
    for (const { cpu_max } of cgroups) {
        const [quota, period] = (cpu_max ?? "max").split(" ");
        if (quota === "max" || !quota || !period) continue;
        cpus = Math.min(cpus, Number(quota) / Number(period));
    }
    return cpus;
}

export function effectiveMemoryMib(
    mem_total_kib: number,
    cgroups: CgroupLimits[],
): number {
    let bytes = mem_total_kib * 1024;
    for (const { memory_max } of cgroups) {
        if (memory_max === null || memory_max === "max") continue;
        bytes = Math.min(bytes, Number(memory_max));
    }
    return Math.floor(bytes / (1024 * 1024));
}

function check(
    name: string,
    ok: boolean,
    expected: string,
    observed: string,
): Check {
    return { name, ok, expected, observed };
}

const resources = (value: Resources) =>
    `${value.vcpus} vCPU, ${value.memory_mib} MiB`;

export function evaluate(
    environment: Environment,
    guest: GuestFacts,
    observation: Observation,
): Check[] {
    const want = environment.resources;
    const cpus = effectiveCpus(observation.nproc, observation.cgroups);
    const memory_mib = effectiveMemoryMib(
        observation.mem_total_kib,
        observation.cgroups,
    );
    const min_memory_mib = Math.floor(want.memory_mib * min_memory_fraction);
    const { chromium, deno } = environment;
    const run = observation.chromium;
    const agent = `HeadlessChrome/${chromium.version.split(".")[0]}.`;
    const fixture_ran = run.exit_code === 0 && !run.timed_out &&
        run.dom.includes("fixture script ran") && run.dom.includes(agent) &&
        run.fixture_requests.includes("GET /");
    return [
        check(
            "provider reports the pinned resources",
            guest.reported.vcpus === want.vcpus &&
                guest.reported.memory_mib === want.memory_mib,
            resources(want),
            resources(guest.reported),
        ),
        check(
            "guest is limited to the pinned vCPUs",
            cpus === want.vcpus,
            `${want.vcpus}`,
            `${cpus} (nproc ${observation.nproc})`,
        ),
        check(
            "guest is limited to the pinned memory",
            memory_mib <= want.memory_mib && memory_mib >= min_memory_mib,
            `${min_memory_mib}–${want.memory_mib} MiB`,
            `${memory_mib} MiB`,
        ),
        check(
            "Deno is the pinned version",
            observation.deno_version === deno.version,
            deno.version,
            observation.deno_version,
        ),
        check(
            "Chromium is the pinned version",
            observation.chromium_version.endsWith(` ${chromium.version}`),
            chromium.version,
            observation.chromium_version,
        ),
        check(
            "headless Chromium ran the local fixture",
            fixture_ran,
            `exit 0, fixture script ran, ${agent}, GET / served`,
            run.timed_out
                ? "timed out"
                : `exit ${run.exit_code}, requests ${
                    run.fixture_requests.join(", ") || "none"
                }${
                    fixture_ran
                        ? ""
                        : `, stderr: ${run.stderr_tail.trim().slice(-300)}`
                }`,
        ),
        check(
            "no Chromium process outlived the run",
            observation.leftover_chromium_pids.length === 0,
            "none",
            observation.leftover_chromium_pids.join(", ") || "none",
        ),
        check(
            "Chromium profile was removed",
            observation.profile_removed,
            "removed",
            observation.profile_removed ? "removed" : "still present",
        ),
    ];
}
