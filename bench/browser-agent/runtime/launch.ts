import { spawn } from "node:child_process";
import { closeSync, openSync } from "node:fs";
import { resolve } from "node:path";
import process from "node:process";

const log = openSync("runtime.log", "a", 0o600);
try {
    const child = spawn(Deno.execPath(), [
        "run",
        "--no-config",
        "--import-map=imports.json",
        "--lock=runtime.lock",
        "--frozen",
        "-A",
        "bench/browser-agent/runtime/worker.ts",
        resolve("runtime-config.json"),
        resolve("control.json"),
    ], { detached: true, stdio: ["ignore", log, log] });
    await new Promise<void>((resolve, reject) => {
        child.once("error", reject);
        child.once("spawn", resolve);
    });
    try {
        await Deno.writeTextFile(
            "runtime-pid.json",
            JSON.stringify({
                pid: child.pid,
                config_path: resolve("runtime-config.json"),
            }),
            { mode: 0o600 },
        );
        child.unref();
    } catch (error) {
        if (child.pid) process.kill(-child.pid, "SIGKILL");
        throw error;
    }
} finally {
    closeSync(log);
}
