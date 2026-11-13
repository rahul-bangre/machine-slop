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

await recordRun({
    run_dir,
    dashboard: new URL("../dashboard/index.html", import.meta.url),
    events,
    timeline: new Timeline(segments),
    out_file,
});
