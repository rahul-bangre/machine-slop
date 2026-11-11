import { Browser } from "./browser.ts";

type Event = { t: number; type: string; [key: string]: unknown };
type Segment = {
    from: number;
    to: number;
    video_ms: number;
    overlay: string | null;
    hold_speed?: number;
};

const [run_dir, out_file] = Deno.args;
if (!run_dir || !out_file) {
    throw new Error("usage: record.ts <run dir> <out.mp4>");
}
const chrome = Deno.env.get("CHROME")!;
const ffmpeg = Deno.env.get("FFMPEG") ?? "ffmpeg";
const fps = Number(Deno.env.get("FPS") ?? 30);
const limit_frames = Number(Deno.env.get("LIMIT_FRAMES") ?? Infinity);

const events: Event[] = (await Deno.readTextFile(`${run_dir}/events.jsonl`))
    .trim().split("\n").map((line) => JSON.parse(line));
const find = (pred: (e: Event) => boolean) => events.find(pred);
const sceneAt = (id: string) =>
    find((e) => e.type === "scene" && e.scene_id === id)?.t ??
        find((e) => e.type === "scene" && e.id === id)!.t;
const done_at = find((e) => e.type === "done")!.t;
const fork_done = find((e) => e.type === "fork")!.t;
const race_end = find((e) => e.type === "race_end")!.t;
const winner = find((e) => e.type === "winner");

const race_ms = race_end - sceneAt("race");
const race_video_ms = Math.min(62_000, Math.max(34_000, race_ms / 6));

const segments: Segment[] = [
    { from: 0, to: 0, video_ms: 4500, overlay: "intro", hold_speed: 1 },
    {
        from: sceneAt("boot"),
        to: sceneAt("setup"),
        video_ms: 3500,
        overlay: null,
    },
    {
        from: sceneAt("setup"),
        to: sceneAt("secrets"),
        video_ms: 6000,
        overlay: null,
    },
    {
        from: sceneAt("secrets"),
        to: sceneAt("snapshot"),
        video_ms: 5500,
        overlay: null,
    },
    {
        from: sceneAt("snapshot"),
        to: sceneAt("fork"),
        video_ms: 4500,
        overlay: null,
    },
    { from: sceneAt("fork"), to: fork_done, video_ms: 4500, overlay: null },
    { from: fork_done, to: sceneAt("race"), video_ms: 3500, overlay: null },
];
if (winner) {
    const before = winner.t - sceneAt("race");
    const before_video = race_video_ms * before / race_ms;
    segments.push(
        {
            from: sceneAt("race"),
            to: winner.t,
            video_ms: before_video,
            overlay: null,
        },
        {
            from: winner.t,
            to: winner.t,
            video_ms: 2500,
            overlay: null,
            hold_speed: 1,
        },
        {
            from: winner.t,
            to: race_end,
            video_ms: Math.max(4000, race_video_ms - before_video),
            overlay: null,
        },
    );
} else {
    segments.push({
        from: sceneAt("race"),
        to: race_end,
        video_ms: race_video_ms,
        overlay: null,
    });
}
segments.push(
    { from: race_end, to: sceneAt("pause"), video_ms: 5000, overlay: null },
    {
        from: sceneAt("pause"),
        to: sceneAt("bill"),
        video_ms: 9000,
        overlay: null,
    },
    {
        from: sceneAt("bill"),
        to: done_at,
        video_ms: 7000,
        overlay: "bill",
        hold_speed: 1,
    },
    {
        from: done_at,
        to: done_at,
        video_ms: 6500,
        overlay: "end",
        hold_speed: 1,
    },
);

function at(
    video_ms: number,
): { t: number; speed: number; overlay: string | null } {
    let start = 0;
    for (const segment of segments) {
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
    const last = segments[segments.length - 1]!;
    return { t: last.to, speed: 1, overlay: last.overlay };
}

function videoTimeOf(t: number): number {
    let start = 0;
    for (const segment of segments) {
        if (segment.to > segment.from && t >= segment.from && t <= segment.to) {
            return start + segment.video_ms * (t - segment.from) /
                    (segment.to - segment.from);
        }
        start += segment.video_ms;
    }
    return t <= 0 ? 0 : start;
}

const total_ms = segments.reduce((sum, segment) => sum + segment.video_ms, 0);
const annotated = events.map((event) => ({
    ...event,
    vt: videoTimeOf(event.t),
}));
const dashboard = await Deno.readTextFile(
    new URL("../dashboard/index.html", import.meta.url),
);
const html = dashboard.replace(
    "<script>\nconst RUN",
    () =>
        `<script>window.RUN = ${
            JSON.stringify({ events: annotated }).replaceAll("<", "\\u003c")
        };</script>\n<script>\nconst RUN`,
);
await Deno.writeTextFile(`${run_dir}/video.html`, html);

const browser = await Browser.launch(chrome);
const page = await browser.page({ width: 1920, height: 1080 });
await page.goto(`file://${run_dir}/video.html`, 1500);
console.log("images preloaded:", await page.evaluate<number>("window.ready"));

const stills = Deno.env.get("STILLS");
if (stills) {
    for (const video_ms of stills.split(",").map(Number)) {
        const { t, speed, overlay } = at(video_ms);
        await page.evaluate(
            `window.renderAt(${t}, ${video_ms}, ${speed}, ${
                JSON.stringify(overlay)
            })`,
        );
        await Deno.writeFile(
            `${out_file}-${video_ms}.jpg`,
            await page.jpeg(85),
        );
    }
    console.log(
        `segments: ${
            segments.map((s) => Math.round(s.video_ms)).join(" ")
        } total ${Math.round(total_ms)}`,
    );
    await browser.close();
    Deno.exit(0);
}

const encoder = new Deno.Command(ffmpeg, {
    args: [
        "-y",
        "-loglevel",
        "error",
        "-f",
        "image2pipe",
        "-framerate",
        String(fps),
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
        out_file,
    ],
    stdin: "piped",
}).spawn();
const writer = encoder.stdin.getWriter();
const frames = Math.min(limit_frames, Math.ceil(total_ms / 1000 * fps));
console.log(`rendering ${frames} frames, ${(total_ms / 1000).toFixed(1)} s`);
const started = performance.now();
for (let frame = 0; frame < frames; frame++) {
    const video_ms = frame * 1000 / fps;
    const { t, speed, overlay } = at(video_ms);
    await page.evaluate(
        `window.renderAt(${t}, ${video_ms}, ${speed}, ${
            JSON.stringify(overlay)
        })`,
    );
    await writer.write(await page.jpeg(92));
    if (frame % 150 === 0) {
        console.log(
            `frame ${frame}/${frames} t=${Math.round(t)} speed=${
                speed.toFixed(2)
            } ${((performance.now() - started) / 1000).toFixed(0)}s`,
        );
    }
}
await writer.close();
const status = await encoder.status;
await browser.close();
if (!status.success) throw new Error("ffmpeg failed");
console.log(`wrote ${out_file}`);
