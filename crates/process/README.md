# Process execution

```ts
import { Machine } from "@machine-slop/sdk";

const machine = new Machine("http://127.0.0.1:3000");
const process = await machine.startProcess({
  program: "/bin/sh",
  args: ["-c", "printf hello"],
  cwd: "/workspace",
});

for await (const event of process.events()) {
  if (event.type === "stdout") console.log(atob(event.data));
}
console.log(await process.wait());
```

Arguments pass directly; `env` overrides the daemon's environment. Optional
`stdin` is initial UTF-8 input, followed by EOF. Output data is base64.

Reconnect with `machine.process(id)` and `events(last_seq + 1)`. Disconnecting
leaves execution running. State lasts until daemon restart or record eviction.
Starts are not idempotent; do not retry a lost start response automatically.

`kill()` requests SIGKILL of the direct child, including when that child is a
shell. Descendants are unaffected. `wait()` returns an observed exit or an
`unknown` outcome. `stdin_complete` reports writes to the pipe, not program
consumption. `output_complete` reports EOF on both output pipes; draining stops
one second after child exit.

Replay retains at most 1 MiB of encoded output and 256 events per process.
Expired cursors return HTTP 410; use `info().first_seq` to resume with loss. A
gap during streaming throws `OutputGap`. The registry holds 128 records, evicts
finished records first, and rejects starts when full of active records.

`make generate` updates SDK types from Rust contracts. This API assumes a
trusted machine; it provides no authentication or workload isolation.
