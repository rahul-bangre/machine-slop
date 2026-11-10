# Browser agent bench

Compares two browser runtimes for a model-driven agent inside the same sandbox:
fresh native accessibility queries (`native`) and Playwright's AI snapshots and
locators (`playwright`). Both support scoped observations, complete modal views,
batched actions, explicit waits and full fallback. This tests a browser runtime
policy, not a modified Chromium or a hosting advantage.

```sh
cargo build -p agentd
deno task bench:chromium
deno task test:browser-agent
deno task bench:browser-agent
```

| Flag                   | Default                                       |
| ---------------------- | --------------------------------------------- |
| `--sandboxes`          | `agentd`; also `machined`, `e2b`, `daytona`   |
| `--engines`            | `playwright,native`                           |
| `--tasks`              | `shop-checkout,dashboard-ticket`              |
| `--rounds`             | `3`                                           |
| `--model`              | `claude-opus-5-5`                             |
| `--max-turns`          | `20`                                          |
| `--agentd-binary`      | `./target/debug/agentd`                       |
| `--agentd-url`         | none; starts a local `agentd`                 |
| `--machined-url`       | `http://127.0.0.1:4001`                       |
| `--e2b-template`       | the record in `bench/env/images/e2b.json`     |
| `--daytona-snapshot`   | the record in `bench/env/images/daytona.json` |
| `--browser-executable` | the pinned local Chromium                     |
| `--deno`               | `deno`                                        |
| `--out`                | `bench-results/browser-agent-<timestamp>/`    |

The model runs through the local `claude` CLI with its subscription login and
tools disabled; no model API key is needed. Each model command is parsed as
browser JSON, never executed as shell code.

## Tasks

The default tasks are for tuning. The held-out
[workflow pack](../browser/README.md#held-out-workflows) is selected explicitly
with
`--tasks=invoice-dispute,member-offboarding,expense-claim,room-rebooking,address-change`.
Freeze the drivers and prompt before running it, and do not tune on its results.
Rounds rotate seeds and engine order, and both engines get the same seed.

## Sandboxes

Every sandbox other than local `agentd` needs Deno 2.9.7 and Chromium
157.0.8103.50 installed, and `--browser-executable` set to the guest path.

- `e2b` and `daytona` default to the [`bench/env`](../env/README.md) builds,
  with 2 CPUs and 4 GiB; use `--browser-executable=/usr/local/bin/chromium`.
  They need `E2B_API_KEY` or `DAYTONA_API_KEY`, and have network access for
  setup while tasks stay on guest loopback.
- `machined` needs the host to itself and destroys every machine it finds. Its
  machines get no network, and the stock image has no browser runtime yet.
- Local `agentd` runs without resource limits.

## Results

The results directory holds `run.json`, `attempts.jsonl` and CLI transcripts.
Failed attempts and cleanup errors stay in the record. Success is checked from
server outcomes, separately from the model's stop reason. Setup, model calls,
tool transport, action, wait and capture phases, report collection and cleanup
are timed separately.

Token proxies use `o200k_base`; actual CLI usage adds scaffolding and possible
side calls. CPU and summed RSS are sampled, include measurement buffers, and
count shared memory repeatedly. No billed cost is inferred.
