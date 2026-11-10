import sys

from lru import LRUCache

failures = []


def expect(label, got, wanted):
    if got != wanted:
        failures.append(f"{label}: got {got!r}, expected {wanted!r}")


for bad in (0, -1, 1.5, "2", None):
    try:
        LRUCache(bad)
        failures.append(f"LRUCache({bad!r}) did not raise ValueError")
    except ValueError:
        pass
    except Exception as error:
        failures.append(f"LRUCache({bad!r}) raised {type(error).__name__}")

cache = LRUCache(2)
expect("empty length", len(cache), 0)
expect("missing key", cache.get("a"), None)
expect("missing key default", cache.get("a", 7), 7)
cache.put("a", 1)
cache.put("b", 2)
expect("order after puts", cache.keys(), ["a", "b"])
expect("get a", cache.get("a"), 1)
expect("get refreshes", cache.keys(), ["b", "a"])
cache.put("c", 3)
expect("evicts least recent", cache.keys(), ["a", "c"])
expect("evicted value", cache.get("b"), None)
expect("miss changes nothing", cache.keys(), ["a", "c"])
cache.put("a", 10)
expect("update refreshes", cache.keys(), ["c", "a"])
expect("update value", cache.get("a"), 10)
expect("length at capacity", len(cache), 2)
cache.put("d", None)
expect("stores None", cache.keys(), ["a", "d"])
expect("None value with default", cache.get("d", "fallback"), None)

if failures:
    print("\n".join(failures))
    sys.exit(1)
