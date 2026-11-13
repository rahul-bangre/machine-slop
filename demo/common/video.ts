import { Browser, type Page } from "./browser.ts";

export type Segment = {
    from: number;
    to: number;
    video_ms: number;
    overlay: string | null;
    hold_speed?: number;
};

export type Moment = { t: number; speed: number; overlay: string | null };

export class Timeline {
    readonly total_ms: number;

    constructor(readonly segments: Segment[]) {
        this.total_ms = segments.reduce((sum, s) => sum + s.video_ms, 0);
    }

    at(video_ms: number): Moment {
        let start = 0;
        for (const segment of this.segments) {
            if (video_ms < start + segment.video_ms) {
                const progress = (video_ms - start) / segment.video_ms;
                const span = segment.to - segment.from;
                return {
                    t: segment.from + span * progress,
                    speed: segment.hold_speed ?? span / segment.video_ms,
                    overlay: segment.overlay,
                };
            }
            start += segment.video_ms;
        }
        const last = this.segments[this.segments.length - 1]!;
        return { t: last.to, speed: 1, overlay: last.overlay };
    }

    videoTimeOf(t: number): number {
        let start = 0;
        for (const segment of this.segments) {
            if (
                segment.to > segment.from && t >= segment.from &&
                t <= segment.to
            ) {
                return start + segment.video_ms * (t - segment.from) /
                        (segment.to - segment.from);
            }
            start += segment.video_ms;
        }
        return t <= 0 ? 0 : start;
    }
}

export function inlineRun(
    dashboard: string,
    events: Record<string, unknown>[],
): string {
    const data = JSON.stringify({ events }).replaceAll("<", "\\u003c");
    if (!/<script>\s*const RUN/.test(dashboard)) {
        throw new Error("Dashboard is missing its run data entry point");
    }
    return dashboard.replace(
        /<script>\s*const RUN/,
        () => `<script>window.RUN = ${data};</script>\n<script>\nconst RUN`,
    );
}

async function renderAt(page: Page, video_ms: number, moment: Moment) {
    await page.evaluate(
        `window.renderAt(${moment.t}, ${video_ms}, ${moment.speed}, ${
            JSON.stringify(moment.overlay)
        })`,
    );
}

export async function writeStills(
    page: Page,
    timeline: Timeline,
    stills: number[],
    prefix: string,
): Promise<void> {
    for (const video_ms of stills) {
        await renderAt(page, video_ms, timeline.at(video_ms));
        await Deno.writeFile(`${prefix}-${video_ms}.jpg`, await page.jpeg(85));
    }
}

export async function encodeVideo(
    page: Page,
    timeline: Timeline,
    options: { ffmpeg: string; out_file: string; fps: number },
): Promise<void> {
    const encoder = new Deno.Command(options.ffmpeg, {
        args: [
            "-y",
            "-loglevel",
            "error",
            "-f",
            "image2pipe",
            "-framerate",
            String(options.fps),
            "-c:v",
            "mjpeg",
            "-i",
            "-",
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-crf",
            "20",
            "-pix_fmt",
            "yuv420p",
            "-movflags",
            "+faststart",
            options.out_file,
        ],
        stdin: "piped",
    }).spawn();
    const writer = encoder.stdin.getWriter();
    const frames = Math.ceil(timeline.total_ms / 1000 * options.fps);
    const started = performance.now();
    for (let frame = 0; frame < frames; frame++) {
        const video_ms = frame * 1000 / options.fps;
        await renderAt(page, video_ms, timeline.at(video_ms));
        await writer.write(await page.jpeg(92));
        if (frame % 300 === 0) {
            console.log(
                `frame ${frame}/${frames} ${
                    ((performance.now() - started) / 1000).toFixed(0)
                }s`,
            );
        }
    }
    await writer.close();
    if (!(await encoder.status).success) throw new Error("ffmpeg failed");
}

export type RunEvent = { t: number; type: string; [key: string]: unknown };

export async function readEvents(run_dir: string): Promise<RunEvent[]> {
    return (await Deno.readTextFile(`${run_dir}/events.jsonl`)).trim().split(
        "\n",
    ).map((line) => JSON.parse(line));
}

export async function recordRun(options: {
    run_dir: string;
    dashboard: URL;
    events: RunEvent[];
    timeline: Timeline;
    out_file: string;
}): Promise<void> {
    const annotated = options.events.map((event) => ({
        ...event,
        vt: options.timeline.videoTimeOf(event.t),
    }));
    const html = inlineRun(
        await Deno.readTextFile(options.dashboard),
        annotated,
    );
    await Deno.writeTextFile(`${options.run_dir}/video.html`, html);
    const browser = await Browser.launch(Deno.env.get("CHROME")!);
    try {
        const page = await browser.page({ width: 1920, height: 1080 });
        await page.goto(`file://${options.run_dir}/video.html`, 1500);
        const ready = await page.evaluate("window.ready");
        if (ready === undefined) {
            throw new Error("Dashboard did not initialize");
        }
        console.log("images preloaded:", ready);
        const stills = Deno.env.get("STILLS");
        if (stills) {
            await writeStills(
                page,
                options.timeline,
                stills.split(",").map(Number),
                options.out_file,
            );
            return;
        }
        console.log(
            `rendering ${(options.timeline.total_ms / 1000).toFixed(1)} s`,
        );
        await encodeVideo(page, options.timeline, {
            ffmpeg: Deno.env.get("FFMPEG") ?? "ffmpeg",
            out_file: options.out_file,
            fps: Number(Deno.env.get("FPS") ?? 30),
        });
        console.log(`wrote ${options.out_file}`);
    } finally {
        await browser.close();
    }
}
