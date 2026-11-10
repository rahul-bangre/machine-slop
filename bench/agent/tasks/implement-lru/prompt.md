Implement the `LRUCache` class in `lru.py`. It is a least-recently-used cache.

- `LRUCache(capacity)` raises `ValueError` unless `capacity` is a positive
  integer.
- `get(key, default=None)` returns the stored value and marks the key as most
  recently used. For a missing key it returns `default` and changes nothing.
- `put(key, value)` inserts or updates the key and marks it as most recently
  used. If the cache then holds more than `capacity` keys, the least recently
  used key is removed.
- `len(cache)` is the number of stored keys.
- `keys()` returns a list of the keys from least to most recently used.

Keep the file name and the class name.
