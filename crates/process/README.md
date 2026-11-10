# Process execution

`agentd` serves this crate over HTTP. This API assumes a trusted machine; it
provides no authentication or workload isolation.

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

| Request                         | SDK                         | Result                                     |
| ------------------------------- | --------------------------- | ------------------------------------------ |
| `POST /processes`               | `machine.startProcess(...)` | Starts a process; 201 with `ProcessInfo`   |
| `GET /processes/{id}`           | `process.info()`            | `ProcessInfo`                              |
| `GET /processes/{id}/wait`      | `process.wait()`            | `ProcessResult` once the process finishes  |
| `GET /processes/{id}/events`    | `process.events(cursor?)`   | NDJSON output events from `?cursor=`       |
| `POST /processes/{id}/kill`     | `process.kill()`            | SIGKILL to the process group; 202          |
| `POST /processes/{id}/stdin`    | `writeStdin`, `closeStdin`  | Appends at `?offset=`, closes with `close` |
| `POST /processes/{id}/terminal` | `process.resize(size)`      | Resizes the pseudo-terminal; 204           |

Every request body is limited to 64 KiB, including the start request with its
`args`, `env` and initial `stdin`. An unknown or evicted id returns 404. Output
data is base64.

## Start

Arguments pass directly; `env` overrides the daemon's environment. Optional
`stdin` is initial UTF-8 input, after which stdin closes unless
`keep_stdin_open` is true. A program that cannot be spawned returns 422. Starts
are not idempotent; do not retry a lost start response automatically.

## Stdin

`writeStdin(offset, data)` appends and returns the next offset;
`closeStdin(offset)` ends input. The offset counts bytes already written, so a
retried request covering written bytes succeeds without writing them again,
including after stdin closes. An offset ahead of the stream, or new bytes after
stdin closes, returns 409. A write returns once its bytes are in the stdin pipe,
not when the program reads them. Offsets identify bytes without comparing them,
so each process has one stdin writer; a writer that reconnects resumes from
`info().stdin_offset`.

## Terminal

With `terminal: { rows, cols }`, the program runs in a new session whose
controlling terminal is a pseudo-terminal of that size, with `TERM` set to
`xterm-256color` unless `env` sets it. Terminal output, including echoed input,
arrives as `stdout` events, and stdin writes are typed into the terminal. Input
stays open until the program exits: `keep_stdin_open: false` returns 422 and
`closeStdin` 409; send `\x04` for end of input in canonical mode. `resize(size)`
returns once the kernel has the new size, which signals `SIGWINCH` to the
foreground group. Resizing a finished or non-terminal process returns 409.

## Lifetime

Each process runs in its own process group. `kill()` sends SIGKILL to the group,
and when the program exits, anything still in its group is killed. A descendant
that leaves the group, such as with `setsid` or as a job-control shell's job, is
not tracked. `wait()` returns an observed exit or an `unknown` outcome.
`stdin_complete` is false when accepted input was never written.
`output_complete` reports EOF on both output pipes; draining stops one second
after the child exits.

Disconnecting leaves execution running. Reconnect with `machine.process(id)` and
`events(last_seq + 1)`. State lasts until the daemon restarts or the record is
evicted. On SIGTERM or SIGINT, `agentd` kills every process before exiting.

Replay keeps at most 1 MiB of encoded output and 256 events per process. An
expired cursor returns 410; resume with loss from `info().first_seq`. A gap
while streaming throws `OutputGap`. The registry holds 128 records: a start when
full evicts the oldest finished record, or returns 503 if none has finished.
