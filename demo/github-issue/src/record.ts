import {
    readEvents,
    recordRun,
    type RunEvent,
    type Segment,
    Timeline,
} from "../../common/video.ts";

const [run_dir, out_file] = Deno.args;
if (!run_dir || !out_file) {
    throw new Error("usage: record.ts <run dir> <out.mp4>");
}

const events = await readEvents(run_dir);
const find = (pred: (e: RunEvent) => boolean) => events.find(pred);
const boot = find((e) => e.type === "scene" && e.id === "boot")!.t;
const work = find((e) => e.type === "scene" && e.id === "work")!.t;
const issue = find((e) => e.type === "egress") ??
    find((e) => e.type === "issue");
const done = find((e) => e.type === "done")!.t;
const work_end = issue?.t ?? done;
const work_video_ms = Math.min(48_000, Math.max(26_000, (work_end - work) / 4));

const segments: Segment[] = [
    { from: boot, to: boot, video_ms: 7500, overlay: "intro", hold_speed: 1 },
    { from: boot, to: work, video_ms: 4500, overlay: null },
    { from: work, to: work_end, video_ms: work_video_ms, overlay: null },
];
if (issue) {
    segments.push(
        {
            from: issue.t,
            to: issue.t,
            video_ms: 5000,
            overlay: null,
            hold_speed: 1,
        },
        { from: issue.t, to: done, video_ms: 4000, overlay: null },
        {
            from: done,
            to: done,
            video_ms: 9000,
            overlay: "result",
            hold_speed: 1,
        },
    );
}

await recordRun({
    run_dir,
    dashboard: new URL("../dashboard/index.html", import.meta.url),
    events,
    timeline: new Timeline(segments),
    out_file,
});
