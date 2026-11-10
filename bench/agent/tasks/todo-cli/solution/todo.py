import json
import os
import sys

STORE = "todos.json"


def load():
    if not os.path.exists(STORE):
        return {"next_id": 1, "items": []}
    with open(STORE) as store:
        return json.load(store)


def save(state):
    with open(STORE, "w") as store:
        json.dump(state, store)


def find(state, raw_id):
    for item in state["items"]:
        if str(item["id"]) == raw_id:
            return item
    print(f"No such item: {raw_id}", file=sys.stderr)
    return None


def main(argv):
    state = load()
    if argv[:1] == ["add"] and len(argv) == 2:
        item = {"id": state["next_id"], "text": argv[1], "done": False}
        state["next_id"] += 1
        state["items"].append(item)
        save(state)
        print(f"Added: {item['text']}")
        return 0
    if argv in (["list"], ["list", "--pending"]):
        for item in state["items"]:
            if item["done"] and "--pending" in argv:
                continue
            mark = "x" if item["done"] else " "
            print(f"{item['id']}. [{mark}] {item['text']}")
        return 0
    if argv[:1] in (["done"], ["remove"]) and len(argv) == 2:
        item = find(state, argv[1])
        if item is None:
            return 1
        if argv[0] == "done":
            item["done"] = True
            print(f"Done: {item['text']}")
        else:
            state["items"].remove(item)
            print(f"Removed: {item['text']}")
        save(state)
        return 0
    print(
        "usage: todo.py add <text> | list [--pending] | done <id> | remove <id>",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
