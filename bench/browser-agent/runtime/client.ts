const [control_path, operation, command = "{}"] = Deno.args;
if (!control_path || !["command", "report", "stop"].includes(operation ?? "")) {
    throw new Error(
        "usage: client.ts <control path> command|report|stop [json]",
    );
}
const control: { port: number; token: string } = JSON.parse(
    await Deno.readTextFile(control_path),
);
const response = await fetch(`http://127.0.0.1:${control.port}/${operation}`, {
    method: "POST",
    headers: {
        authorization: `Bearer ${control.token}`,
        "content-type": "application/json",
    },
    body: command,
    signal: AbortSignal.timeout(35_000),
});
const text = await response.text();
console.log(text);
if (!response.ok) Deno.exit(1);
