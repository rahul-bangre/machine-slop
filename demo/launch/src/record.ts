import { readEvents, recordRun, Timeline } from "../../common/video.ts";

const [run_dir, out_file] = Deno.args;
if (!run_dir || !out_file) {
    throw new Error("usage: record.ts <race run directory> <output.mp4>");
}
const race = await readEvents(run_dir);
const research_dir = new URL("../../fork-research/run/", import.meta.url);
const research = await readEvents(research_dir.pathname);
const images: Record<string, string> = {};
for (const event of research.filter((event) => event.type === "frame")) {
    const file = String(event.file);
    const data = await Deno.readFile(new URL(file, research_dir));
    let binary = "";
    for (const byte of data) binary += String.fromCharCode(byte);
    images[file] = `data:image/jpeg;base64,${btoa(binary)}`;
}
for (const type of ["checkpoint", "forked", "winner", "done"]) {
    if (!race.some((event) => event.type === type)) {
        throw new Error(`Incomplete race: missing ${type}`);
    }
}
if (race.some((event) => ["error", "copy_failed"].includes(event.type))) {
    throw new Error("Cannot render a failed race");
}
const checkpoint = race.find((event) => event.type === "checkpoint")!;
const copies = race.filter((event) => event.type === "copy");
if (copies.length !== 10) throw new Error("This film requires ten real copies");
for (const copy of copies) {
    const verified = race.find((event) =>
        event.type === "verified" && event.lane === copy.lane
    );
    const first = race.find((event) =>
        event.type === "out" && event.lane === copy.lane
    );
    if (
        verified?.s !== checkpoint.s || verified?.pid !== checkpoint.pid ||
        JSON.stringify(verified?.path) !== JSON.stringify(checkpoint.path) ||
        first?.s !== Number(checkpoint.s) + 1
    ) {
        throw new Error(
            `Copy ${copy.lane} did not continue the saved checkpoint`,
        );
    }
}
await recordRun({
    run_dir,
    dashboard: new URL("../dashboard/index.html", import.meta.url),
    events: [{ t: 0, type: "bundle", race, research, images }],
    timeline: new Timeline([
        { from: 0, to: 64_000, video_ms: 64_000, overlay: null },
    ]),
    out_file,
});
