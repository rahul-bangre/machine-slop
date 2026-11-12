import { readEvents, recordRun, Timeline } from "../../common/video.ts";

const [run_dir, out_file] = Deno.args;
if (!run_dir || !out_file) {
    throw new Error("usage: record.ts <run directory> <output.mp4>");
}
const events = await readEvents(run_dir);
const copies = events.filter((event) => event.type === "copy");
const baseline = events.find((event) => event.type === "reproduced");
if (
    copies.length !== 3 || !baseline ||
    !events.some((event) => event.type === "selection") ||
    !events.some((event) => event.type === "pr")
) {
    throw new Error("Recording is missing the real fork, selection, or PR");
}
for (const copy of copies) {
    const state = copy.state as Record<string, unknown>;
    if (
        state.session !== baseline.session || state.pid !== baseline.pid ||
        state.phase !== baseline.phase
    ) throw new Error("Copy lost the live reproduction");
    if (
        !events.some((event) =>
            event.type === "tests" && event.lane === copy.lane &&
            event.passed === 29 && event.failed === 0
        )
    ) throw new Error(`Lane ${copy.lane} did not pass the SDK suite`);
}
const images: Record<string, string> = {};
for (const name of ["origin", "A", "B", "C", "pr"]) {
    const bytes = await Deno.readFile(`${run_dir}/frames/${name}.png`);
    let binary = "";
    for (const byte of bytes) binary += String.fromCharCode(byte);
    images[name] = `data:image/png;base64,${btoa(binary)}`;
}
const patches: Record<string, string> = {};
for (const lane of ["A", "B", "C"]) {
    patches[lane] = await Deno.readTextFile(`${run_dir}/${lane}.patch`);
}
await recordRun({
    run_dir,
    dashboard: new URL("../dashboard/index.html", import.meta.url),
    events: [{ t: 0, type: "bundle", events, images, patches }],
    timeline: new Timeline([{
        from: 0,
        to: 64_000,
        video_ms: 64_000,
        overlay: null,
    }]),
    out_file,
});
