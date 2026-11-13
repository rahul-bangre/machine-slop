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
const sceneAt = (id: string) =>
    find((e) => e.type === "scene" && e.id === id)!.t;
const boot = find((e) => e.type === "machine" && e.lane === "main")!;
const origin = boot.t - Number(boot.ms);
const decision = find((e) => e.type === "fork_decision")!;
const parallel = sceneAt("parallel");
const merge = sceneAt("merge");
const resumed = find((e) => e.type === "machine" && e.resumed === true)!;
const issue = find((e) => e.type === "issue");
const done = find((e) => e.type === "done")!.t;
const merge_end = issue?.t ?? done;

const segments: Segment[] = [
    {
        from: origin,
        to: origin,
        video_ms: 8000,
        overlay: "intro",
        hold_speed: 1,
    },
    { from: origin, to: decision.t, video_ms: 17_000, overlay: null },
    {
        from: decision.t,
        to: decision.t,
        video_ms: 3500,
        overlay: null,
        hold_speed: 1,
    },
    { from: decision.t, to: parallel, video_ms: 8000, overlay: null },
    { from: parallel, to: merge, video_ms: 22_000, overlay: null },
    {
        from: resumed.t,
        to: resumed.t,
        video_ms: 7000,
        overlay: "join",
        hold_speed: 1,
    },
    { from: resumed.t, to: merge_end, video_ms: 9000, overlay: null },
];
if (issue) {
    segments.push(
        {
            from: issue.t,
            to: issue.t,
            video_ms: 3500,
            overlay: null,
            hold_speed: 1,
        },
        { from: issue.t, to: done, video_ms: 2000, overlay: null },
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
