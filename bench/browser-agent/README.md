# Browser agent bench

Compare fresh native accessibility queries with Playwright's AI snapshots and
locators inside the same sandbox. Both support scoped observations, complete
modal views, batched actions, explicit waits and full fallback. This tests a
browser runtime policy, not a modified Chromium or a hosting advantage.

```sh
cargo build -p agentd
deno task bench:chromium
deno task test:browser-agent
deno task bench:browser-agent --rounds=3
```

Uses the local Claude CLI subscription login, with tools disabled. The default
model is pinned in the run metadata; no model API key is required. Each model
command is parsed as browser JSON, never executed as shell code.

Checkout and dashboard are tuning tasks. Select the independently graded pack
explicitly with
`--tasks=invoice-dispute,member-offboarding,expense-claim,room-rebooking,address-change`.
Rounds rotate seeds and arm order; each pair gets the same seed. Freeze the
drivers and prompt before held-out evaluation. Do not tune on those results.

For E2B, Daytona or machined, supply `--sandboxes=...`, a prepared environment
with Deno 2.9.7 and Chromium 157.0.8103.50, and
`--browser-executable=/guest/path`. `--deno=/guest/path` and
`--e2b-template=...` select installed runtimes. Pin equal CPU/memory limits for
cross-provider comparisons. machined requires an exclusive host and destroys
every machine it finds; its stock image currently has no network or browser
runtime. Local agentd runs without host resource limits.

E2B and Daytona default to the committed `bench/env/images/` records (2 CPUs, 4
GiB). Use `--browser-executable=/usr/local/bin/chromium` with those images;
`--daytona-snapshot=...` selects another prepared snapshot. Both providers allow
network access for dependency setup; tasks stay on guest loopback.

Raw attempts, observations, CLI transcripts and configuration go to
`bench-results/`. Failed attempts and cleanup errors remain visible. Success is
checked from server outcomes, separately from the model's stop reason. Setup,
model calls, tool transport, action/wait/capture phases, report collection and
cleanup are recorded separately. Token proxies use `o200k_base`; actual CLI
usage includes scaffolding and possible side calls. Sampled runtime/browser CPU
and summed RSS include measurement buffers and are approximate; shared memory is
counted repeatedly. No billed cost is inferred from these measurements.
