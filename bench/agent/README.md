# Agent bench

Runs the same coding agent on the same tasks in different sandboxes, to compare
what each sandbox costs an agent end to end.

```sh
export ANTHROPIC_API_KEY=...
make build
deno task bench:agent --agentd-binary ./target/release/agentd

export E2B_API_KEY=...
deno task bench:agent --sandboxes agentd,e2b --rounds 3
deno task bench:agent --agentd-url http://127.0.0.1:3001 --tasks todo-cli
```

Results land in `bench-results/agent-<timestamp>/`:

- `run.json`: the model settings, sandboxes, limits and tasks
- `attempts.jsonl`: one record per attempt, failures included
- `transcripts/`: the full conversation of every attempt
- `summary.md`: pass counts and cost per task, per sandbox

Every attempt calls the model, so a run costs money: one agent session per task,
per sandbox, per round.

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

The model is fixed in `src/model.ts`: one model id, one effort level, one system
prompt, and a single bash tool. Each command runs in a fresh shell in the task
directory. Refusal fallback is off, because a fallback would change the model
mid-run; a response from any other model fails the attempt.

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
