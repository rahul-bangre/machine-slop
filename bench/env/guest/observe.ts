export type CgroupLimits = {
    path: string;
    cpu_max: string | null;
    memory_max: string | null;
};

export type ChromiumRun = {
    exit_code: number | null;
    timed_out: boolean;
    dom: string;
    stderr_tail: string;
    fixture_requests: string[];
};

export type Observation = {
    deno_version: string;
    chromium_version: string;
    nproc: number;
    mem_total_kib: number;
    cgroups: CgroupLimits[];
    chromium: ChromiumRun;
    leftover_chromium_pids: number[];
    profile_removed: boolean;
};

const chromium_timeout_ms = 60_000;
const leftover_grace_ms = 5_000;

async function command(program: string, args: string[]): Promise<string> {
    const output = await new Deno.Command(program, {
        args,
        stdin: "null",
        stdout: "piped",
        stderr: "inherit",
    }).output();
    if (!output.success) {
        throw new Error(`${program} ${args.join(" ")} exited ${output.code}`);
    }
    return new TextDecoder().decode(output.stdout).trim();
}

async function readOrNull(path: string): Promise<string | null> {
    try {
        return (await Deno.readTextFile(path)).trim();
    } catch (error) {
        if (error instanceof Deno.errors.NotFound) return null;
        throw error;
    }
}

async function cgroupLimitsUpToRoot(): Promise<CgroupLimits[]> {
    const membership = await Deno.readTextFile("/proc/self/cgroup");
    const own = membership.match(/^0::(\S*)$/m)?.[1];
    if (own === undefined) return [];
    const limits: CgroupLimits[] = [];
    let path = own === "/" ? "" : own;
    while (true) {
        const directory = `/sys/fs/cgroup${path}`;
        limits.push({
            path: path || "/",
            cpu_max: await readOrNull(`${directory}/cpu.max`),
            memory_max: await readOrNull(`${directory}/memory.max`),
        });
        if (path === "") return limits;
        path = path.slice(0, path.lastIndexOf("/"));
    }
}

async function memTotalKib(): Promise<number> {
    const meminfo = await Deno.readTextFile("/proc/meminfo");
    const kib = meminfo.match(/^MemTotal:\s+(\d+) kB$/m)?.[1];
    if (!kib) throw new Error("/proc/meminfo has no MemTotal");
    return Number(kib);
}

async function chromiumPids(install_root: string): Promise<number[]> {
    const pids: number[] = [];
    for await (const entry of Deno.readDir("/proc")) {
        if (!/^\d+$/.test(entry.name)) continue;
        const executable = await Deno.readLink(`/proc/${entry.name}/exe`)
            .catch(() => "");
        if (executable.startsWith(install_root)) pids.push(Number(entry.name));
    }
    return pids;
}

async function tail(stream: ReadableStream<Uint8Array>): Promise<string> {
    const text = await new Response(stream).text();
    return text.slice(-2000);
}

async function openFixture(
    chromium: string,
    fixture: string,
    profile: string,
): Promise<ChromiumRun> {
    const page = await Deno.readTextFile(fixture);
    const fixture_requests: string[] = [];
    const server = Deno.serve(
        { hostname: "127.0.0.1", port: 0, onListen: () => {} },
        (request) => {
            const path = new URL(request.url).pathname;
            fixture_requests.push(`${request.method} ${path}`);
            if (path !== "/") return new Response(null, { status: 404 });
            return new Response(page, {
                headers: { "content-type": "text/html; charset=utf-8" },
            });
        },
    );
    try {
        const child = new Deno.Command(chromium, {
            args: [
                "--headless=new",
                "--no-sandbox",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                `--user-data-dir=${profile}`,
                "--dump-dom",
                `http://127.0.0.1:${server.addr.port}/`,
            ],
            stdin: "null",
            stdout: "piped",
            stderr: "piped",
        }).spawn();
        let timed_out = false;
        const limit = setTimeout(() => {
            timed_out = true;
            child.kill("SIGKILL");
        }, chromium_timeout_ms);
        const [dom, stderr_tail, status] = await Promise.all([
            new Response(child.stdout).text(),
            tail(child.stderr),
            child.status,
        ]);
        clearTimeout(limit);
        return {
            exit_code: status.signal === null ? status.code : null,
            timed_out,
            dom,
            stderr_tail,
            fixture_requests,
        };
    } finally {
        await server.shutdown();
    }
}

async function reapChromium(install_root: string): Promise<number[]> {
    const deadline = Date.now() + leftover_grace_ms;
    let leftover = await chromiumPids(install_root);
    while (leftover.length > 0 && Date.now() < deadline) {
        await new Promise((resolve) => setTimeout(resolve, 100));
        leftover = await chromiumPids(install_root);
    }
    for (const pid of leftover) {
        try {
            Deno.kill(pid, "SIGKILL");
        } catch (error) {
            if (!(error instanceof Deno.errors.NotFound)) throw error;
        }
    }
    return leftover;
}

export async function observe(
    chromium: string,
    fixture: string,
): Promise<Observation> {
    const install_root = `${
        (await Deno.realPath(chromium)).replace(/\/[^/]+$/, "")
    }/`;
    const chromium_version = await command(chromium, ["--version"]);
    const profile = await Deno.makeTempDir({ prefix: "env-smoke-profile-" });
    let run: ChromiumRun;
    let leftover_chromium_pids: number[];
    try {
        run = await openFixture(chromium, fixture, profile);
    } finally {
        leftover_chromium_pids = await reapChromium(install_root);
        await Deno.remove(profile, { recursive: true }).catch(() => {});
    }
    const profile_removed = await Deno.stat(profile).then(
        () => false,
        (error) => error instanceof Deno.errors.NotFound,
    );
    return {
        deno_version: Deno.version.deno,
        chromium_version,
        nproc: Number(await command("nproc", [])),
        mem_total_kib: await memTotalKib(),
        cgroups: await cgroupLimitsUpToRoot(),
        chromium: run,
        leftover_chromium_pids,
        profile_removed,
    };
}

if (import.meta.main) {
    const [chromium, fixture] = Deno.args;
    if (!chromium || !fixture) {
        throw new Error("usage: observe.ts <chromium> <fixture.html>");
    }
    console.log(JSON.stringify(await observe(chromium, fixture)));
}
