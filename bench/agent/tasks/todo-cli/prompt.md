`todo.py` is a small command line to-do list that stores its items in
`todos.json` in the current directory. It supports `add <text>` and `list`.

Extend it with:

- `done <id>`: marks the item as done and prints `Done: <text>`.
- `remove <id>`: deletes the item and prints `Removed: <text>`. Ids of the
  remaining items do not change, and a removed id is never reused.
- `list --pending`: lists only the items that are not done.

`list` prints one line per item in id order, as `<id>. [ ] <text>` for a pending
item and `<id>. [x] <text>` for a done one.

For `done` and `remove`, an id that does not exist prints `No such item: <id>`
to stderr and exits with status 1, leaving the list unchanged.

Keep the existing behaviour of `add` and `list`, and keep the file name.
