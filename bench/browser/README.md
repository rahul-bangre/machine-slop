# browser-bench

Measures what a browser costs an agent, so a candidate browser or observation
path can be judged against unmodified headless Chromium.

```sh
deno task bench:chromium
deno task bench
deno task bench --arms stock-ax,stock-html --rounds 10 --tasks shop-checkout
deno task bench --live
```

Results land in `bench-results/<timestamp>/`:

- `run.json`: host, pinned conditions, arms, observation format and tokenizer
- `attempts.jsonl`: one record per attempt, failed attempts and retries included
- `observations/`: every observation exactly as captured
- `summary.md`: reliability, cost per task, ratio to the first arm

## What is compared

An arm is a browser plus an observation format. Each attempt uses one arm in a
fresh browser with a fresh profile, because some formats change what the browser
does for the rest of the session.

Arms and browsers are declared in `bench.json`. A candidate browser is another
entry under `browsers` with its own `executable`, `product` and `args`. A
candidate observation path is another entry in `src/observers.ts`.

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
- observation: capturing the observation
- think: a fixed idle pause standing in for model inference, with the CPU the
  browser burns during it

Memory is the peak of summed RSS across the process tree, sampled every 200 ms.
Shared pages are counted once per process, so it overstates absolute use.

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

An observer may send only what changed since its previous observation. It then
supplies `rebuild(previous_state, sent)`, a pure function. The harness applies
it to the state it holds itself, runs the content check against that rebuilt
state, and counts tokens on what was sent. A delta that does not apply fails the
attempt. Both the sent delta and the rebuilt state are saved.

`ax-diff` is the reference: a full accessibility tree first, then a line delta
diffed from a full capture. It saves tokens, not capture work, so it shows what
delta delivery is worth before a native implementation exists.

The rebuilt state is checked for the expected text only. It is not compared line
for line with a fresh full capture.

Failed attempts are kept. Timings are summarised over passing attempts only, and
the reliability table reports how many runs passed first time, how many passed
after retries, and every failed attempt by kind.

## Order

Arms run interleaved per task, and their order rotates every round and every
task, so each arm runs in each position equally often.
