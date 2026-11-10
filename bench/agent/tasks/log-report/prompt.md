`access.log` holds one request per line:

```
<timestamp> <method> <path> <status> <latency_ms>
```

Write `report.py`. Run as `python3 report.py`, it reads `access.log` from the
current directory and writes `report.json` there, containing an object with
exactly these keys:

- `total_requests`: the number of requests
- `status_counts`: an object mapping each status code, as a string, to how many
  requests had it
- `top_paths`: the three most requested paths as `[path, count]` pairs, most
  requested first, ties broken by path in ascending order
- `p95_latency_ms`: the 95th percentile latency by the nearest-rank method: sort
  the latencies ascending and take the one at position `ceil(0.95 * n)`,
  counting from 1
- `slowest_path`: the path with the highest mean latency, ties broken by path in
  ascending order
