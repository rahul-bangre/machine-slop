import os
import shutil
import subprocess
import sys
import tempfile

source = os.path.abspath("todo.py")
failures = []


def todo(directory, *arguments):
    ran = subprocess.run(
        [sys.executable, "todo.py", *arguments],
        cwd=directory,
        capture_output=True,
        text=True,
    )
    return ran.returncode, ran.stdout.strip().splitlines(), ran.stderr.strip()


def expect(label, got, wanted):
    if got != wanted:
        failures.append(f"{label}: got {got!r}, expected {wanted!r}")


with tempfile.TemporaryDirectory() as directory:
    shutil.copy(source, directory)
    expect("empty list", todo(directory, "list"), (0, [], ""))
    expect("add first", todo(directory, "add", "buy milk"), (0, ["Added: buy milk"], ""))
    todo(directory, "add", "write report")
    todo(directory, "add", "call Sam")
    expect(
        "list all pending",
        todo(directory, "list")[1],
        ["1. [ ] buy milk", "2. [ ] write report", "3. [ ] call Sam"],
    )
    expect("done", todo(directory, "done", "2"), (0, ["Done: write report"], ""))
    expect(
        "list shows done",
        todo(directory, "list")[1],
        ["1. [ ] buy milk", "2. [x] write report", "3. [ ] call Sam"],
    )
    expect(
        "list pending",
        todo(directory, "list", "--pending"),
        (0, ["1. [ ] buy milk", "3. [ ] call Sam"], ""),
    )
    expect("done unknown", todo(directory, "done", "9"), (1, [], "No such item: 9"))
    expect("remove", todo(directory, "remove", "1"), (0, ["Removed: buy milk"], ""))
    expect("remove unknown", todo(directory, "remove", "1"), (1, [], "No such item: 1"))
    expect(
        "ids unchanged after remove",
        todo(directory, "list")[1],
        ["2. [x] write report", "3. [ ] call Sam"],
    )
    todo(directory, "add", "book flights")
    expect(
        "removed id not reused",
        todo(directory, "list")[1],
        ["2. [x] write report", "3. [ ] call Sam", "4. [ ] book flights"],
    )
    expect(
        "list pending after changes",
        todo(directory, "list", "--pending")[1],
        ["3. [ ] call Sam", "4. [ ] book flights"],
    )

if failures:
    print("\n".join(failures))
    sys.exit(1)
