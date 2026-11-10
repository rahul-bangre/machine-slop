import json
import os
import sys

STORE = "todos.json"


def load():
    if not os.path.exists(STORE):
        return []
    with open(STORE) as store:
        return json.load(store)


def save(items):
    with open(STORE, "w") as store:
        json.dump(items, store)


def main(argv):
    items = load()
    if argv[:1] == ["add"] and len(argv) == 2:
        item = {"id": len(items) + 1, "text": argv[1], "done": False}
        items.append(item)
        save(items)
        print(f"Added: {item['text']}")
        return 0
    if argv == ["list"]:
        for item in items:
            print(f"{item['id']}. [ ] {item['text']}")
        return 0
    print("usage: todo.py add <text> | list", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
