from collections import OrderedDict


class LRUCache:
    def __init__(self, capacity):
        if isinstance(capacity, bool) or not isinstance(capacity, int) or capacity < 1:
            raise ValueError("capacity must be a positive integer")
        self._capacity = capacity
        self._entries = OrderedDict()

    def get(self, key, default=None):
        if key not in self._entries:
            return default
        self._entries.move_to_end(key)
        return self._entries[key]

    def put(self, key, value):
        self._entries[key] = value
        self._entries.move_to_end(key)
        if len(self._entries) > self._capacity:
            self._entries.popitem(last=False)

    def __len__(self):
        return len(self._entries)

    def keys(self):
        return list(self._entries)
