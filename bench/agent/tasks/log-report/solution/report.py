import json
import math
from collections import Counter, defaultdict

statuses = Counter()
paths = Counter()
latencies = []
latency_by_path = defaultdict(list)
with open("access.log") as log:
    for line in log:
        if not line.strip():
            continue
        _, _, path, status, latency = line.split()
        statuses[status] += 1
        paths[path] += 1
        latencies.append(int(latency))
        latency_by_path[path].append(int(latency))

latencies.sort()
ranked = sorted(paths.items(), key=lambda item: (-item[1], item[0]))
slowest = min(
    latency_by_path,
    key=lambda path: (-sum(latency_by_path[path]) / len(latency_by_path[path]), path),
)
with open("report.json", "w") as out:
    json.dump(
        {
            "total_requests": len(latencies),
            "status_counts": dict(statuses),
            "top_paths": [list(item) for item in ranked[:3]],
            "p95_latency_ms": latencies[math.ceil(0.95 * len(latencies)) - 1],
            "slowest_path": slowest,
        },
        out,
    )
