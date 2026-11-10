# Browser bench

Measures what a browser costs an agent, so a candidate browser or observation
path can be judged against unmodified headless Chromium.

```sh
deno task bench:chromium
deno task bench
deno task bench --arms stock-ax,stock-html --rounds 10 --tasks shop-checkout
deno task bench --live
```

`bench:chromium` downloads the pinned Chromium for `darwin-aarch64` or
`linux-x86_64`.

| Flag                | Default                                                                             |
| ------------------- | ----------------------------------------------------------------------------------- |
| `--config`          | `bench.json`                                                                        |
| `--arms`            | every arm in the config                                                             |
| `--tasks`           | `shop-checkout`, `docs-lookup`, `dashboard-ticket`                                  |
| `--live`            | off; adds `live-wikipedia-search`, `live-hacker-news-paging`, `live-mdn-navigation` |
| `--rounds`          | `5`                                                                                 |
| `--retries`         | `1`                                                                                 |
| `--think-ms`        | `500`                                                                               |
| `--step-timeout-ms` | `20000`                                                                             |
| `--out`             | `bench-results/<timestamp>/`                                                        |

The results directory holds:

- `run.json`: host, pinned conditions, arms, observation format and tokenizer
- `attempts.jsonl`: one record per attempt, failed attempts and retries included
- `observations/`: every observation exactly as captured
- `summary.md`: reliability, cost per task, ratio to the first arm

## What is compared

An arm is a browser plus an observation format. Each attempt uses one arm in a
fresh browser with a fresh profile, because some formats change what the browser
does for the rest of the session.

Arms and browsers are declared in `bench.json`: `stock-ax`, `stock-ax-diff`,
`stock-html` and `stock-screenshot` pair stock Chromium with each observer in
`src/observers.ts`. A candidate browser is another entry under `browsers` with
its own `executable`, `product` and `args`. A candidate observation path is
another entry in `src/observers.ts`.

## What is pinned

- Chromium: Chrome for Testing at the version and archive hash in
  `chromium.json`. A browser that reports a different product aborts the run.
- Launch flags, viewport, device scale factor, locale and time zone, in
  `src/browser.ts`.
- Task content: the fixture sites are served locally from deterministic data, on
  a fixed origin.
- Live tasks (`--live`) run against real sites and are labelled `live-unpinned`.
  Their content changes between runs.

Host CPU and memory are recorded, not limited.

## What is measured

Per attempt: startup (time to the DevTools endpoint, time to a usable page),
total CPU seconds and peak memory of the browser process tree.

Per step, timed separately:

- action: dispatching the input
- readiness: from the end of the action until the step's condition holds, the
  document is complete and no request is in flight
- readiness confirmation: the harness then waits for that to stay true for 250
  ms before observing. This is harness policy, not browser cost, and is reported
  on its own line so the phases add up to the time actually waited
- observation: capturing the observation, including tokenizing it
- think: a fixed idle pause standing in for model inference, with the CPU the
  browser burns during it

Memory is the peak of summed RSS across the process tree, sampled every 200 ms.
Shared pages are counted once per process, so it overstates absolute use. CPU is
the summed CPU time of the same tree; time a process uses after its last sample
is not counted.

Token counts are recorded with the observation format and the tokenizer that
produced them. Screenshot tokens are an estimate from pixel area.

## What counts as success

Fixture tasks are judged from the fixture server's request log, not from the
browser: the order or ticket the server received must be the right one. Live
tasks are judged from page state.

Each step also names text the agent must be able to see. An observation that
lacks it fails the attempt, so an observation path that is cheap because it
drops content does not pass. Screenshots are not content-checked.

## Delta observations

Each observation says whether it is the full state or a delta against the state
the harness already holds. For a delta, the observer supplies a pure
`rebuild(previous_state, delta)`. The harness applies it to its own copy of the
state, runs the content check against the rebuilt state, and counts tokens on
what was sent. A delta that does not apply fails the attempt. Both the sent
delta and the rebuilt state are saved.

`ax-diff` is the reference: it captures the full accessibility tree every step
and sends either that or a line delta, whichever is fewer tokens. It saves
tokens, not capture work, so it shows what delta delivery is worth before a
native implementation exists.

The rebuilt state is checked for the expected text only. It is not compared line
for line with a fresh full capture.

Failed attempts are kept. Timings are summarised over passing attempts only, and
the reliability table reports how many runs passed first time, how many passed
after retries, every failed attempt by kind, and how many observations were
content-checked.

Only failures of the browser or the task count against an arm: timeouts, failed
actions, rejected observations, wrong outcomes and browser crashes. An error in
the harness itself stops the run.

Only one run can use the fixture origin at a time; a second run exits at
startup.

## Order

Arms run interleaved per task, and their order rotates every round and every
task, so each arm runs in each position equally often.

## Held-out workflows

`src/workflows/` is a pack of small web apps for a model-driven agent: the agent
gets a goal in plain language and has to reach it in the browser. The pack is
held out. Do not use it to tune prompts, observers or the model loop; tune on
the fixture tasks instead.

`src/workflows/mod.ts` exports them as `workflows`. Each has an `id`, the
capabilities it `covers`, a list of `seeds`, and `open(seed)`, which returns a
prompt and a site with fresh state:

- `site.handle(request)` serves the app and its JSON API. Every path is rooted
  at `/`, and the start page is `/`. Pages fetch their data from the API, so the
  server owns all state.
- `site.outcome()` resolves to `null` when the goal was met, otherwise the
  reason it was not. It first waits for every request the site has already
  received to finish, so a submit still in flight when the agent stops is
  counted. It reads only the server's committed records, never the page or a
  request log, and fails on no action, the wrong target, wrong values, and extra
  or duplicate side effects. The server validates every write and rejects
  malformed ones instead of committing them.
- The prompt states the goal and the facts needed (names, amounts, credentials,
  fallback rule), not how to reach it: no URLs, selectors, labels or steps.

A seed builds one scenario, and the prompt, the served data and the expected
outcome all come from it. Seeds change the people, amounts, targets and which
slot conflicts, not just labels. Latency is fixed per endpoint, dates are fixed,
and nothing is random at request time.

| Workflow             | Covers                           | Goal                                                                                                                     |
| -------------------- | -------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `invoice-dispute`    | async loading, dialogs           | Dispute the largest unpaid invoice from a customer in a given month. Invoices load slowly, a page at a time.             |
| `member-offboarding` | dialogs                          | Remove a member and hand their projects to someone else, past a blocking announcement and a typed confirmation.          |
| `expense-claim`      | multi-step form, async loading   | File a claim through a four-step wizard with a section that depends on the category and a cost-centre search.            |
| `room-rebooking`     | recovery, dialogs, async loading | Book a room. The requested slot shows as free but is taken when booked; the prompt gives the fallback rule.              |
| `address-change`     | recovery                         | Change the shipping address. The session expires on the first save, so the agent signs in again and re-enters the input. |

Dialogs are in-page `<dialog>` elements opened with `showModal()`. The apps
never use `alert`, `confirm`, `prompt` or `beforeunload`, because how the model
loop handles native dialogs is unspecified. Every control works with click, fill
and Enter: choices are radio buttons rather than `<select>`, dates are text
fields, and forms start empty so nothing has to be cleared.

`test/workflows.test.ts` drives each site through the HTTP requests its pages
make: a reference solution must pass and representative mistakes must fail.
