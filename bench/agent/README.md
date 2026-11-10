# Agent bench

Runs the same coding agent on the same tasks in different sandboxes, to compare
what each sandbox costs an agent end to end.

```sh
cargo build -p agentd
deno task bench:agent

export E2B_API_KEY=...
deno task bench:agent --sandboxes agentd,e2b --tasks todo-cli

make vm-start
deno task bench:agent --sandboxes machined
deno task bench:agent --agentd-url http://127.0.0.1:3001
```

| Flag                   | Default                            |
| ---------------------- | ---------------------------------- |
| `--sandboxes`          | `agentd`                           |
| `--model`              | `claude-cli:claude-opus-5-5`       |
| `--tasks`              | every task under `tasks/`          |
| `--rounds`             | `3`                                |
| `--max-turns`          | `30`                               |
| `--command-timeout-ms` | `60000`                            |
| `--agentd-binary`      | `./target/debug/agentd`            |
| `--agentd-url`         | none; starts a local `agentd`      |
| `--machined-url`       | `http://127.0.0.1:4001`            |
| `--e2b-template`       | `base`                             |
| `--out`                | `bench-results/agent-<timestamp>/` |

The `machined` provider sends `SLOP_API_KEY` as its API key.

The results directory holds:

- `run.json`: the model settings, sandboxes, limits and tasks
- `attempts.jsonl`: one record per attempt, failures included
- `transcripts/`: the full conversation of every attempt
- `summary.md`: pass counts and cost per task, per sandbox

Each attempt is one agent session, so a run spends API credit, or subscription
usage with `claude-cli`.

## Sandboxes

`src/sandbox.ts` defines the provider interface; a new sandbox is one adapter.

- `agentd` starts a local `agentd` for each attempt, or uses a running one with
  `--agentd-url`. Commands run unisolated as the `agentd` user, so use it only
  for trusted tasks. It is a baseline for command latency without a machine
  around it.
- `machined` gets a new Firecracker microVM per attempt from a running
  `machined`, which hands out one it booted ahead of time when one is ready, and
  destroys it afterwards. Machines get an empty `allowed_hosts`, so they have no
  network. The bench needs the host to itself: it refuses to start if machines
  are already running, and destroys every machine it finds before each create
  and when the run ends, which also cleans up after a lost create response.
- `e2b` starts a sandbox from `--e2b-template` per attempt. Needs `E2B_API_KEY`.
- `daytona` starts a `python:3.12-slim` sandbox with 2 CPUs and 4 GiB per
  attempt. Needs `DAYTONA_API_KEY`. Its create time includes image preparation,
  so it is not a raw boot measurement.

`machined`, `e2b` and `daytona` create a machine per attempt, so their create
times are comparable. `e2b` and `daytona` sandboxes have network access;
`machined` ones do not.

## Model

`--model <provider>:<model id>` pins one model for the whole run.

- `claude-cli` runs the `claude` command headless with its own login and no
  tools: each reply is schema-validated JSON listing the commands to run. The
  CLI adds its own scaffolding tokens and saves each session in its history.
- `anthropic` calls the Messages API with a bash tool. Needs
  `ANTHROPIC_API_KEY`.
- `openai` calls Chat Completions with a bash function tool. Needs
  `OPENAI_API_KEY`.

All three share the same system prompt, and each command runs in a fresh shell
in the task directory. An attempt served by more than one model fails. Token
accounting differs between providers, so compare sandboxes under one model and
over several rounds.

## Tasks

Each directory under `tasks/` holds:

- `prompt.md`: what the agent is asked to do
- `files/`: the starting files, written into the sandbox
- `check.py`: run in the sandbox after the agent finishes; exit 0 is a pass
- `solution/`: a reference solution, used only by the tests

The tests check that each checker rejects the starting files and accepts the
reference solution.

## Measurements and failures

Per attempt: sandbox create, task setup, model turns, commands, time in sandbox
commands, time waiting on the model, tokens, checker and close time, and total
wall time.

An attempt fails as one of:

- `checker`: the agent finished but the checker did not exit 0
- `turn_limit`: no final answer within `--max-turns`
- `model`: a refusal, a truncated response, or the API being unreachable, rate
  limited or erroring after the client's retries
- `sandbox`: the sandbox could not be created, written to, run or closed,
  including a command that does not stop after its timeout, and any command
  timeout on `daytona`, which deletes the sandbox

Missing model credentials, malformed requests and a sandbox provider that is not
ready stop the run instead.

Sandboxes run interleaved per task, with the order rotating every round and
every task. They are released when the run ends or is interrupted with Ctrl-C.
