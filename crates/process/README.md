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
`stdin` is initial UTF-8 input, after which stdin closes unless
`keep_stdin_open` is true.

`writeStdin(offset, data)` appends bytes and returns the next offset;
`closeStdin(offset)` ends input. The offset counts bytes already written, so a
retried write with the same offset and bytes is written once, and an offset
ahead of the stream returns HTTP 409. A write returns once its bytes are in the
stdin pipe, not when the program reads them. Writes after stdin closes return
409. `info()` reports `stdin_offset` and `stdin_open` for resuming. Output data
is base64.

Reconnect with `machine.process(id)` and `events(last_seq + 1)`. Disconnecting
leaves execution running. State lasts until daemon restart or record eviction.
Starts are not idempotent; do not retry a lost start response automatically.

Each process runs in its own process group. `kill()` sends SIGKILL to the group,
and when the program exits, anything still running in its group is killed. A
descendant that leaves the group, for example with `setsid`, is not tracked.
`wait()` returns an observed exit or an `unknown` outcome. `stdin_complete` is
false when accepted input was never written because the program exited or closed
stdin. `output_complete` reports EOF on both output pipes; draining stops one
second after child exit.

Replay retains at most 1 MiB of encoded output and 256 events per process.
Expired cursors return HTTP 410; use `info().first_seq` to resume with loss. A
gap during streaming throws `OutputGap`. The registry holds 128 records, evicts
finished records first, and rejects starts when full of active records.

`make generate` updates SDK types from Rust contracts. This API assumes a
trusted machine; it provides no authentication or workload isolation.
