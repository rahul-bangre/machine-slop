# Fork the future

A 64-second launch cut: one running Python search becomes ten independent
Firecracker machines, followed by the recorded agent research task from
`../fork-research`.

[Watch the 1080p film with original music](media/fork-the-future.mp4).

## What is real

The maze is drawn from process output captured inside the machines. The source
stops at an explicit checkpoint before pausing. Each copy inherits the same
checkpoint file and running Python process, then receives a different search
seed through stdin. No process is restarted in the copies. The driver compares
every inherited checkpoint with the original before proceeding.

This recorded run used ten 128 MiB VMs on a 2-vCPU host with 3.7 GiB RAM. Pause
took 244 ms, snapshot 19 ms, and all ten copies were ready and verified in 433
ms. The measured interval from pause request to all copies ready was 696 ms.
These are local measurements for this small process, not Chromium or agent
startup numbers. The search inputs demonstrate independent execution; they do
not guarantee a speedup on arbitrary searches.

The agent segment reuses the actual event log and browser captures in
`../fork-research/run`. It is explicitly labeled as an edited recording. That
run forked three 640 MiB browser machines and three agent conversations, then
posted issue #29. Rendering this film creates no new issues and makes no model
calls.

## Art direction

Large type introduces one action at a time. A light opening cuts to a dark
checkpoint, then a camera pullback reveals all ten machines before their paths
diverge. An original synthesized score marks the transitions. The real browser
captures provide the payoff after the abstract search example.

The launch research included the first-party
[Vercel Ship production breakdown](https://basement.studio/post/behind-the-ship):
its story-driven reveal and deliberate timing informed the pacing. The film uses
its own graphics, animation, and music.

## Capture and render

Use the repository's pinned Deno version. Start a standalone demo host with
spares disabled, a tenant quota for ten machines plus a paused source, and
sufficient memory and disk. The driver uses an empty egress allowlist and cleans
up machines and the snapshot in `finally`.

```sh
COPIES=10 MACHINED_URL=http://127.0.0.1:4100 API_KEY_FILE=/path/to/key \
  deno run -A demo/launch/src/race.ts /absolute/run-directory

CHROME=/path/to/chrome FFMPEG=/path/to/ffmpeg \
  deno run -A demo/launch/src/record.ts \
  /absolute/run-directory /absolute/silent.mp4

python3 demo/launch/src/score.py /absolute/score.wav

ffmpeg -y -i /absolute/silent.mp4 -i /absolute/score.wav \
  -c:v copy -c:a aac -b:a 192k -shortest -movflags +faststart \
  /absolute/fork-the-future.mp4
```

To render the checked-in run, use this directory's absolute `run/` path.
`STILLS=2000,10500,45000` renders sample JPGs instead of video. The soundtrack
uses only Python's standard library; it contains no licensed samples.
