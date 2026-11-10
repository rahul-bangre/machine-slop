export type ProcessRow = {
    pid: number;
    ppid: number;
    rssBytes: number;
    cpuSeconds: number;
};

export type UsageMark = { cpuSeconds: number; peakRssSumBytes: number };

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
        const [pid, ppid, rssKib, time] = line.trim().split(/\s+/);
        if (!pid || !ppid || !rssKib || !time) return [];
        return [{
            pid: Number(pid),
            ppid: Number(ppid),
            rssBytes: Number(rssKib) * 1024,
            cpuSeconds: parsePsCpuTime(time),
        }];
    });
}

export function parseLinuxStat(
    stat: string,
    ticksPerSecond: number,
    pageBytes: number,
): ProcessRow {
    const fields = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
    const field = (index: number) => Number(fields[index - 3]);
    return {
        pid: Number(stat.slice(0, stat.indexOf(" "))),
        ppid: field(4),
        rssBytes: field(24) * pageBytes,
        cpuSeconds: (field(14) + field(15)) / ticksPerSecond,
    };
}

export function processTree(rows: ProcessRow[], rootPid: number): ProcessRow[] {
    const members = new Set([rootPid]);
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

let linuxUnits: Promise<{ ticksPerSecond: number; pageBytes: number }> | null =
    null;

async function readLinuxRows(): Promise<ProcessRow[]> {
    linuxUnits ??= Promise.all([
        commandOutput("getconf", ["CLK_TCK"]),
        commandOutput("getconf", ["PAGESIZE"]),
    ]).then(([ticks, page]) => ({
        ticksPerSecond: Number(ticks),
        pageBytes: Number(page),
    }));
    const { ticksPerSecond, pageBytes } = await linuxUnits;
    const rows: ProcessRow[] = [];
    for await (const entry of Deno.readDir("/proc")) {
        if (!/^\d+$/.test(entry.name)) continue;
        try {
            const stat = await Deno.readTextFile(`/proc/${entry.name}/stat`);
            rows.push(parseLinuxStat(stat, ticksPerSecond, pageBytes));
        } catch (error) {
            if (!(error instanceof Deno.errors.NotFound)) throw error;
        }
    }
    return rows;
}

export const readProcessRows = Deno.build.os === "linux"
    ? readLinuxRows
    : readPsRows;

export const cpuTimeResolutionSeconds = 0.01;

export class UsageMeter {
    private readonly cpuByPid = new Map<number, number>();
    private lifetimePeak = 0;
    private windowPeak = 0;
    private timer: ReturnType<typeof setInterval> | undefined;

    constructor(
        private readonly rootPid: number,
        private readonly readRows: () => Promise<ProcessRow[]> =
            readProcessRows,
    ) {}

    start(intervalMs: number): void {
        this.timer = setInterval(() => void this.sample(), intervalMs);
    }

    stop(): void {
        clearInterval(this.timer);
    }

    async sample(): Promise<void> {
        const tree = processTree(await this.readRows(), this.rootPid);
        let rssSum = 0;
        for (const row of tree) {
            rssSum += row.rssBytes;
            const seen = this.cpuByPid.get(row.pid) ?? 0;
            this.cpuByPid.set(row.pid, Math.max(seen, row.cpuSeconds));
        }
        this.windowPeak = Math.max(this.windowPeak, rssSum);
        this.lifetimePeak = Math.max(this.lifetimePeak, rssSum);
    }

    async mark(): Promise<UsageMark> {
        await this.sample();
        const mark = {
            cpuSeconds: this.cpuSeconds(),
            peakRssSumBytes: this.windowPeak,
        };
        this.windowPeak = 0;
        return mark;
    }

    cpuSeconds(): number {
        let total = 0;
        for (const seconds of this.cpuByPid.values()) total += seconds;
        return total;
    }

    peakRssSumBytes(): number {
        return this.lifetimePeak;
    }
}
