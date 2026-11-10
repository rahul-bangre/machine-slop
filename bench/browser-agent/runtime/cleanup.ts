import { execFileSync } from "node:child_process";
import process from "node:process";

const owner: { pid: number; config_path: string } = JSON.parse(
    await Deno.readTextFile("runtime-pid.json"),
);
const command = String(
    execFileSync("ps", ["-p", String(owner.pid), "-o", "command="]),
);
if (!command.includes(owner.config_path)) {
    throw new Error(
        "runtime PID no longer identifies this attempt; refusing to signal it",
    );
}
process.kill(-owner.pid, "SIGKILL");
