import json
import os
import random
import select
import sys
import time

WIDTH, HEIGHT = 32, 18


def carve(seed):
    rng = random.Random(seed)
    open_walls = set()
    seen = {(0, 0)}
    stack = [(0, 0)]
    while stack:
        x, y = stack[-1]
        options = [
            (x + dx, y + dy)
            for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1))
            if 0 <= x + dx < WIDTH and 0 <= y + dy < HEIGHT and (x + dx, y + dy) not in seen
        ]
        if not options:
            stack.pop()
            continue
        nxt = rng.choice(options)
        open_walls.add(frozenset(((x, y), nxt)))
        seen.add(nxt)
        stack.append(nxt)
    for _ in range(WIDTH * HEIGHT // 6):
        x, y = rng.randrange(WIDTH - 1), rng.randrange(HEIGHT - 1)
        dx, dy = rng.choice(((1, 0), (0, 1)))
        open_walls.add(frozenset(((x, y), (x + dx, y + dy))))
    return open_walls


def emit(record):
    sys.stdout.write(json.dumps(record, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def read_branch(step, path):
    if select.select([sys.stdin], [], [], 0)[0]:
        line = sys.stdin.readline().strip()
        if line == "freeze":
            checkpoint = {"checkpoint": True, "s": step, "pid": os.getpid(), "path": path}
            with open("/workspace/checkpoint.json", "w") as saved:
                json.dump(checkpoint, saved)
            emit(checkpoint)
            line = sys.stdin.readline().strip()
        if line.isdigit():
            return int(line)
    return None


maze_seed = int(sys.argv[1])
walk_seed = int(sys.argv[2])
step_seconds = float(sys.argv[3])
walls = carve(maze_seed)
goal = (WIDTH - 1, HEIGHT - 1)
emit({"maze": sorted([list(a) + list(b) for a, b in (sorted(w) for w in walls)]), "w": WIDTH, "h": HEIGHT, "pid": os.getpid()})

rng = random.Random(walk_seed)
started = time.monotonic()
branch = None
path = [(0, 0)]
visited = {(0, 0)}
step = 0
while path[-1] != goal:
    chosen = read_branch(step, path)
    if chosen is not None:
        branch = chosen
        if chosen > 0:
            rng = random.Random(chosen * 7919 + step)
    x, y = path[-1]
    options = [
        (x + dx, y + dy)
        for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1))
        if frozenset(((x, y), (x + dx, y + dy))) in walls and (x + dx, y + dy) not in visited
    ]
    step += 1
    if options:
        nxt = rng.choice(options)
        visited.add(nxt)
        path.append(nxt)
        emit({"s": step, "t": round((time.monotonic() - started) * 1000), "b": branch, "go": nxt})
    else:
        path.pop()
        emit({"s": step, "t": round((time.monotonic() - started) * 1000), "b": branch, "back": path[-1]})
    time.sleep(step_seconds)
emit({"s": step, "t": round((time.monotonic() - started) * 1000), "b": branch, "found": True, "length": len(path)})
