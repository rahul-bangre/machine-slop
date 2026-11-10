# Agent bench

Runs the same coding agent on the same tasks in different sandboxes, to compare
what each sandbox costs an agent end to end.

```sh
make build
deno task bench:agent --agentd-binary ./target/release/agentd

export E2B_API_KEY=...
deno task bench:agent --sandboxes agentd,e2b --rounds 3
deno task bench:agent --agentd-url http://127.0.0.1:3001 --tasks todo-cli

make vm-start
deno task bench:agent --sandboxes machined --rounds 3
deno task bench:agent --model openai:<model id>
```

Results land in `bench-results/agent-<timestamp>/`:

- `run.json`: the model settings, sandboxes, limits and tasks
- `attempts.jsonl`: one record per attempt, failures included
- `transcripts/`: the full conversation of every attempt
- `summary.md`: pass counts and cost per task, per sandbox

Every attempt calls the model: one agent session per task, per sandbox, per
round. That is API spend, or subscription usage with `claude-cli`.

## Sandboxes

A sandbox is three operations: write a file, run a shell script with a timeout,
and close. `src/sandbox.ts` defines the interface; a new provider is one
adapter.

- `machined` asks a running `machined` (`--machined-url`, by default the one
  `make vm-start` forwards to `127.0.0.1:4001`) for a new Firecracker microVM
  per attempt, and destroys it afterwards. It creates machines with an empty
  `allowed_hosts`, so they have no network and runs do not depend on outside
  services. Its create time is a cold boot until `agentd` answers, plus the
  requests around it; the boot time `machined` itself reports is recorded
  separately. The bench needs the host to itself: it refuses to start if
  machines are already running, and destroys every machine it finds before each
  create and when the run ends. That is how a machine from a lost create
  response gets cleaned up.
- `agentd` starts a local `agentd` for each attempt, or uses a running one with
  `--agentd-url`. It has no isolation: commands run as the `agentd` user on its
  host, so use it only for trusted tasks. Its create time is a daemon start or a
  new directory, not a machine boot.
- `e2b` starts a new E2B sandbox from `--e2b-template` for each attempt.

`machined` and `e2b` both boot a machine per attempt, so their create times are
comparable. `agentd` alone is a baseline for command latency without a machine
around it.

## Model

`--model <provider>:<model id>` pins one model for the whole run. The default is
`claude-cli:claude-opus-5-5`.

- `claude-cli` runs the `claude` command headless, so it uses that CLI's login
  and needs no API key. The CLI gets no tools: each reply is schema-validated
  JSON listing the commands to run, and the next message carries their output.
  The CLI adds its own scaffolding tokens, may make small side calls on another
  model, and saves each session in its history.
- `anthropic` calls the Messages API with a bash tool. It needs
  `ANTHROPIC_API_KEY`. Refusal fallback is off, because a fallback would change
  the model mid-run.
- `openai` calls Chat Completions with a bash function tool. It needs
  `OPENAI_API_KEY`.

All three share the same sandbox rules in the system prompt, and each command
runs in a fresh shell in the task directory. An attempt whose turns are served
by more than one model fails.

Results from different providers are not comparable with each other: the tool
protocol and token accounting differ. Compare sandboxes under one model.

The model is not deterministic. Turn counts and token use vary between attempts,
so compare sandboxes over several rounds.

## Tasks

Each directory under `tasks/` holds:

- `prompt.md`: what the agent is asked to do
- `files/`: the starting files, written into the sandbox
- `check.py`: the checker, written into the sandbox and run only after the agent
  has finished. Exit status 0 is a pass
- `solution/`: a reference solution, used only by the tests

The tests prove each checker rejects the starting files and accepts the
reference solution, by driving a scripted model through a real `agentd`.

## What is measured

Per attempt: sandbox create, task file setup, model turns, commands, time spent
in sandbox commands, time spent waiting on the model, tokens, checker and close
time, and total wall time.

## What counts as a failure

- `checker`: the agent finished but the checker did not exit 0
- `turn_limit`: no final answer within `--max-turns`
- `model`: a refusal, a truncated response, or the API being unreachable or rate
  limited after the client's retries
- `sandbox`: the sandbox could not be created, written to, run or closed

Missing or rejected credentials, malformed requests and a sandbox provider that
is not ready stop the run instead of counting as failures.

A command that passes its timeout is killed. If it still has not stopped a few
seconds later, the attempt fails as a `sandbox` failure instead of waiting.

Sandboxes are released when the run ends and when it is interrupted with Ctrl-C.

Sandboxes run interleaved per task, and their order rotates every round and
every task.

## Daytona

```sh
export DAYTONA_API_KEY=...
deno task bench:agent --sandboxes daytona --rounds 3
```

Uses pinned `@daytona/sdk` 0.231.0, image `python:3.12-slim`, 2 CPUs and 4 GiB
RAM per attempt. `DAYTONA_API_URL` and `DAYTONA_TARGET` use SDK defaults.
Creation includes image preparation and work directory setup; it is not a raw VM
boot measurement. Image tags can change remotely.

Creation has a 120 second remote timeout and 125 second local deadline. Setup
and file writes have 30 second deadlines. Commands carry a remote timeout in
whole seconds (rounded up plus one second of grace), plus the exact requested
local deadline. A local command timeout invalidates the sandbox and waits for
confirmed deletion before returning `timed_out`. Further commands fail.
Transport errors also invalidate and delete the sandbox; they remain failures
rather than being guessed to be timeouts. A failed deletion is a sandbox
failure, retained for cleanup retry. The SDK exposes one command output string,
recorded in `stdout`; `stderr` is empty. Partial output is unavailable on
timeout.

Cleanup addresses only random names generated by this provider instance,
including failed creates whose responses were lost. It never enumerates account
sandboxes. Deletion requests wait for destruction, with a 35 second local
deadline; name lookups have a 30 second deadline. A late successful create
triggers another cleanup attempt. Local deadlines do not cancel API requests:
under service failure cleanup can remain uncertain, and process exit prevents
late callbacks. A 30 minute server TTL is a fallback, not proof of immediate
cleanup. Reclaim reports unresolved cleanup errors (the bench's existing
shutdown handler ignores provider reclaim errors). No live cloud compatibility
test is part of the credential-free contract tests.

API contracts:
[creation, resources and deletion](https://www.daytona.io/docs/en/typescript-sdk/daytona/),
[command execution](https://www.daytona.io/docs/en/typescript-sdk/process/).
