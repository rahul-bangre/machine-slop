import json
import os
import subprocess
import sys

if os.path.exists("report.json"):
    os.remove("report.json")
if not os.path.exists("report.py"):
    print("report.py does not exist")
    sys.exit(1)
ran = subprocess.run([sys.executable, "report.py"], capture_output=True, text=True)
if ran.returncode != 0:
    print(f"report.py exited {ran.returncode}: {ran.stderr[-500:]}")
    sys.exit(1)
try:
    with open("report.json") as report_file:
        report = json.load(report_file)
except (OSError, ValueError) as error:
    print(f"report.json is unreadable: {error}")
    sys.exit(1)

rows = [line.split() for line in open("access.log") if line.strip()]
count = len(rows)
by_status = {}
by_path = {}
for _, _, path, status, latency in rows:
    by_status[status] = by_status.get(status, 0) + 1
    by_path.setdefault(path, []).append(int(latency))
ordered = sorted(int(row[4]) for row in rows)
rank = -(-95 * count // 100)
expected = {
    "total_requests": count,
    "status_counts": by_status,
    "top_paths": [
        [path, len(values)]
        for path, values in sorted(
            by_path.items(), key=lambda item: (-len(item[1]), item[0])
        )[:3]
    ],
    "p95_latency_ms": ordered[rank - 1],
    "slowest_path": sorted(
        by_path, key=lambda path: (-sum(by_path[path]) / len(by_path[path]), path)
    )[0],
}

if not isinstance(report, dict):
    print("report.json is not an object")
    sys.exit(1)
wrong = [
    f"{key}: got {report.get(key)!r}, expected {value!r}"
    for key, value in expected.items()
    if report.get(key) != value
]
extra = sorted(set(report) - set(expected))
if extra:
    wrong.append(f"unexpected keys: {extra}")
if wrong:
    print("\n".join(wrong))
    sys.exit(1)
