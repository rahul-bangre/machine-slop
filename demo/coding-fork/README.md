# One bug. Three fixes.

[Watch the 64-second coding film](media/one-bug-three-fixes.mp4).

This run found and fixed a current bug in machine-slop's TypeScript SDK. When a
remote endpoint accepted TCP but never answered the HTTP upgrade, closing its
port forward waited forever. The result is
[PR #30](https://github.com/rahul-bangre/machine-slop/pull/30).

## The recorded work

One machine contains the repository, Deno, an agentd test binary, a running dev
server, and a listening inspector endpoint. A real SDK call remains stuck in
`forward.close()`. Three new shutdown tests fail; established bidirectional
traffic still passes.

The source is paused and snapshotted. Three Firecracker machines restore that
snapshot. Before edits, the controller checks that every copy has the same debug
session UUID, server PID, and `cleanup_pending` state. Each has its own copied
files and connections. Three Codex agents inherit the shared investigation
context and use the VM bridge to read, edit, and test independently. Agent
reasoning runs on the host; their repository work and tests run inside their
VMs.

All three candidates passed the four regression cases and the full 29-test SDK
suite. Their production diffs were:

| Attempt | Approach              | Added / removed | SDK tests |
| ------- | --------------------- | --------------- | --------- |
| A       | Socket ownership      | +9 / -3         | 29 passed |
| B       | Forward lifetime      | +12 / -3        | 29 passed |
| C       | Pending-task tracking | +20 / -3        | 29 passed |

A was selected after review. It uses one forward-owned cancellation controller
and checks the late-handshake race with the smallest production diff. The actual
patch and four regression tests became PR #30. Host review cleaned up the test
teardown for lint; the recorded production patch is unchanged.

The captured original failure remains pending while patches are tested in fresh
processes. Applying a source patch does not rewrite the already-running
JavaScript closure. The film shows the new test results and submitted patch, and
does not claim that the original pending promise was hot-patched.

## Timing and evidence

Three 384 MiB VMs ran on a 2-vCPU host with 3.7 GiB RAM. Pause took 933 ms,
snapshot 269 ms, and all three restored copies answered their state checks in
815 ms. The full measured operation took 2.018 seconds. These are local
measurements of this captured workspace.

`run/events.jsonl` contains the commands, outputs, state checks, test results,
and patch collection. `run/A.patch`, `B.patch`, and `C.patch` preserve all three
attempts. `run/frames` contains actual browser captures of the debug workspace
and PR. The film presents code excerpts and measured results with edited timing.
The branch-work interval shown in 18 seconds lasted 83.4 seconds in the capture.
No outcomes or failing competitors were scripted. All captured machines and the
snapshot were cleaned up after patch collection.

## Capture helpers

Use the pinned Deno version and a standalone demo machined on port 4100 with
spares disabled, the demo image, and a tenant with room for three 384 MiB VMs.
The controller reads `~/machine-slop/demo/api-key` and uploads the host Deno and
`~/projects/machine-slop/target/debug/agentd`. Build agentd before setup.

`SOURCE_COMMIT` defaults to `0d85366`, the main revision where this bug was
found. Setup archives that source and adds the immutable regression fixture. It
starts the captured server with an inspector on guest port 9229 and denies VM
egress.

Run the controller from this checkout under a user service so it survives
terminal or agent-session cleanup:

```sh
systemd-run --user --unit=slop-coding-control --working-directory="$PWD" \
  "$(command -v deno)" run -A demo/coding-fork/src/control.ts /absolute/run-directory

curl -H 'content-type: application/json' -d '{}' localhost:4300/setup

deno run -A demo/coding-fork/src/vm.ts origin \
  'cat packages/sdk/src/forward.ts; cat packages/sdk/src/tunnel.ts'

curl -H 'content-type: application/json' -d '{}' localhost:4300/fork
```

Give each coding agent its own lane, A, B, or C. All repository reads, edits,
and tests go through `src/vm.ts <lane> '<shell command>'`. Tests are immutable.
The agent task is to fix the captured shutdown hang while preserving working
bidirectional and half-close behavior. The three starting lenses are socket
ownership, lifecycle cancellation, and pending-task coordination. Agents can
change their approach based on evidence.

Each agent runs the regression file, the complete SDK test directory, and type
checking. Collect each patch with `POST /collect` and JSON `{ "lane": "A" }`,
then review before selecting one. Capture the browser on forwarded ports
4280–4283 for origin, A, B, and C. Source shutdown closes the origin forward
before pause; its replacement copies have separate forward listeners.

Stop the controller after collection; its SIGTERM handler cleans up all machines
and the snapshot:

```sh
systemctl --user stop slop-coding-control
```

## Render the saved capture

Rendering uses saved data and makes no model calls or GitHub writes.

```sh
CHROME=/path/to/chrome FFMPEG=/path/to/ffmpeg \
  deno run -A demo/coding-fork/src/record.ts \
  "$PWD/demo/coding-fork/run" /absolute/silent.mp4

python3 demo/launch/src/score.py /absolute/score.wav

ffmpeg -y -i /absolute/silent.mp4 -i /absolute/score.wav \
  -c:v copy -c:a aac -b:a 192k -shortest -movflags +faststart \
  /absolute/one-bug-three-fixes.mp4
```

The original synthesized score is generated by the earlier launch score helper.
`STILLS=2000,14000,38000,55000` writes sample frames instead of a video.
