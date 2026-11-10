export type ProcessRow = {
    pid: number;
    ppid: number;
    rss_bytes: number;
    cpu_seconds: number;
};

export function parsePsCpuTime(text: string): number {
    const [clock = "", days] = text.split("-").reverse();
    const seconds = clock.split(":").reduce(
        (total, part) => total * 60 + Number(part),
        0,
    );
    return seconds + Number(days ?? 0) * 86_400;
}

export function parsePsRows(output: string): ProcessRow[] {
    return output.split("\n").flatMap((line) => {
        const [pid, ppid, rss_kib, time] = line.trim().split(/\s+/);
        if (!pid || !ppid || !rss_kib || !time) return [];
        return [{
            pid: Number(pid),
            ppid: Number(ppid),
            rss_bytes: Number(rss_kib) * 1024,
            cpu_seconds: parsePsCpuTime(time),
        }];
    });
}

export function parseLinuxStat(
    stat: string,
    ticks_per_second: number,
    page_bytes: number,
): ProcessRow {
    const fields = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
    const field = (index: number) => Number(fields[index - 3]);
    return {
        pid: Number(stat.slice(0, stat.indexOf(" "))),
        ppid: field(4),
        rss_bytes: field(24) * page_bytes,
        cpu_seconds: (field(14) + field(15)) / ticks_per_second,
    };
}

export function processTree(
    rows: ProcessRow[],
    root_pid: number,
): ProcessRow[] {
    const members = new Set([root_pid]);
    for (let grew = true; grew;) {
        grew = false;
        for (const row of rows) {
            if (members.has(row.ppid) && !members.has(row.pid)) {
                members.add(row.pid);
                grew = true;
            }
        }
    }
    return rows.filter((row) => members.has(row.pid));
}

async function commandOutput(command: string, args: string[]): Promise<string> {
    const { stdout } = await new Deno.Command(command, {
        args,
        stdout: "piped",
        stderr: "null",
    }).output();
    return new TextDecoder().decode(stdout);
}

async function readPsRows(): Promise<ProcessRow[]> {
    return parsePsRows(
        await commandOutput("ps", ["-axo", "pid=,ppid=,rss=,time="]),
    );
}

let linux_units:
    | Promise<{ ticks_per_second: number; page_bytes: number }>
    | null = null;

async function readLinuxRows(): Promise<ProcessRow[]> {
    linux_units ??= Promise.all([
        commandOutput("getconf", ["CLK_TCK"]),
        commandOutput("getconf", ["PAGESIZE"]),
    ]).then(([ticks, page]) => ({
        ticks_per_second: Number(ticks),
        page_bytes: Number(page),
    }));
    const { ticks_per_second, page_bytes } = await linux_units;
    const rows: ProcessRow[] = [];
    for await (const entry of Deno.readDir("/proc")) {
        if (!/^\d+$/.test(entry.name)) continue;
        try {
            const stat = await Deno.readTextFile(`/proc/${entry.name}/stat`);
            rows.push(parseLinuxStat(stat, ticks_per_second, page_bytes));
        } catch (error) {
            if (!(error instanceof Deno.errors.NotFound)) throw error;
        }
    }
    return rows;
}

export const readProcessRows = Deno.build.os === "linux"
    ? readLinuxRows
    : readPsRows;

export const cpu_time_resolution_seconds = 0.01;

export class UsageMeter {
    private readonly cpu_by_pid = new Map<number, number>();
    private peak_rss_sum = 0;
    private timer: ReturnType<typeof setInterval> | undefined;

    constructor(
        private readonly root_pid: number,
        private readonly readRows: () => Promise<ProcessRow[]> =
            readProcessRows,
    ) {}

    start(interval_ms: number): void {
        this.timer = setInterval(() => void this.sample(), interval_ms);
    }

    stop(): void {
        clearInterval(this.timer);
    }

    async sample(): Promise<void> {
        const tree = processTree(await this.readRows(), this.root_pid);
        let rss_sum = 0;
        for (const row of tree) {
            rss_sum += row.rss_bytes;
            const seen = this.cpu_by_pid.get(row.pid) ?? 0;
            this.cpu_by_pid.set(row.pid, Math.max(seen, row.cpu_seconds));
        }
        this.peak_rss_sum = Math.max(this.peak_rss_sum, rss_sum);
    }

    async cpuSecondsNow(): Promise<number> {
        await this.sample();
        let total = 0;
        for (const seconds of this.cpu_by_pid.values()) total += seconds;
        return total;
    }

    peakRssSumBytes(): number {
        return this.peak_rss_sum;
    }
}
