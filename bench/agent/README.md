# Agent bench

Runs the same coding agent on the same tasks in different sandboxes, to compare
what each sandbox costs an agent end to end.

```sh
make build
deno task bench:agent --agentd-binary ./target/release/agentd

export E2B_API_KEY=...
deno task bench:agent --sandboxes agentd,e2b --rounds 3
deno task bench:agent --agentd-url http://127.0.0.1:3001 --tasks todo-cli
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

- `agentd` starts a local `agentd` for each attempt, or uses a running one with
  `--agentd-url`. It has no isolation: commands run as the `agentd` user on its
  host, so point it at an `agentd` inside a VM for anything but trusted tasks.
  Its create time is a daemon start or a new directory, not a machine boot.
- `e2b` starts a new E2B sandbox from `--e2b-template` for each attempt.

Until `agentd` runs inside a machine it boots itself, create time is not
comparable between the two. Command latency is.

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

Missing or rejected credentials and malformed requests stop the run instead of
counting as failures.

Sandboxes run interleaved per task, and their order rotates every round and
every task.
