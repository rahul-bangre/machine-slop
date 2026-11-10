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

`writeStdin(offset, data)` appends at most 64 KiB and returns the next offset;
`closeStdin(offset)` ends input. The offset counts bytes already written. A
retried request covering bytes already written succeeds without writing them
again, including after stdin closes, and an offset ahead of the stream returns
HTTP 409. A write returns once its bytes are in the stdin pipe, not when the
program reads them. New bytes after stdin closes return 409. An offset
identifies bytes without comparing them, so each process has one stdin writer; a
writer that reconnects resumes from `info().stdin_offset`. Output data is
base64.

Reconnect with `machine.process(id)` and `events(last_seq + 1)`. Disconnecting
leaves execution running. State lasts until daemon restart or record eviction.
Starts are not idempotent; do not retry a lost start response automatically.

With `terminal: { rows, cols }`, the program runs in a new session whose
controlling terminal is a pseudo-terminal of that size, with `TERM` set to
`xterm-256color` unless `env` sets it. Its stdout and stderr are the terminal,
so terminal output, including echoed input, arrives as `stdout` events. Stdin
writes are typed into the terminal and keep the same offset rules. A terminal's
input stays open until the program exits: `keep_stdin_open: false` is rejected
with HTTP 422 and `closeStdin` with HTTP 409; send `\x04` for end of input in
canonical mode. `resize(size)` returns once the kernel has the new size, which
also signals `SIGWINCH` to the terminal's foreground group; `info().terminal`
reports the current size, and resizing a finished or non-terminal process
returns HTTP 409.

Each process runs in its own process group. `kill()` sends SIGKILL to the group,
and when the program exits, anything still running in its group is killed. A
descendant that leaves the group, for example with `setsid` or a job-control
shell's job in a terminal, is not tracked. `wait()` returns an observed exit or
an `unknown` outcome. `stdin_complete` is false when accepted input was never
written because the program exited or closed stdin. `output_complete` reports
EOF on both output pipes; draining stops one second after child exit.

Replay retains at most 1 MiB of encoded output and 256 events per process.
Expired cursors return HTTP 410; use `info().first_seq` to resume with loss. A
gap during streaming throws `OutputGap`. The registry holds 128 records, evicts
finished records first, and rejects starts when full of active records.

`make generate` updates SDK types from Rust contracts. This API assumes a
trusted machine; it provides no authentication or workload isolation.
