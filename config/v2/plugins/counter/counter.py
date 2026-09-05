#!/usr/bin/env python3
"""The smallest useful galdeck plugin.

The protocol is line-delimited JSON on stdin and stdout, which is why this is
forty lines of Python with no dependencies rather than a binding to something.

The daemon says hello; you answer `ready`. Then it tells you when your keys
appear, disappear and are pressed, and you tell it what to put on them.
"""

import json
import sys


def send(message):
    sys.stdout.write(json.dumps(message) + "\n")
    # Without this the daemon sees nothing until the buffer fills, which looks
    # exactly like a plugin that has stopped working.
    sys.stdout.flush()


def main():
    counts = {}
    # Whatever the key's `options` said, so one plugin can serve several keys.
    labels = {}

    for line in sys.stdin:
        try:
            message = json.loads(line)
        except ValueError:
            continue

        kind = message.get("type")
        key = message.get("key")

        if kind == "hello":
            send({"type": "ready", "name": "Counter"})
        elif kind == "appear":
            labels[key] = message.get("options", {}).get("label", "count")
            counts.setdefault(key, 0)
            send({"type": "set_text", "key": key, "text": f"{labels[key]}\n{counts[key]}"[:16]})
            send({"type": "set_text", "key": key, "text": str(counts[key])})
        elif kind == "press":
            counts[key] = counts.get(key, 0) + 1
            send({"type": "set_text", "key": key, "text": str(counts[key])})
            # A theme token rather than a literal, so the plugin stays inside
            # whatever palette the user is using.
            send({"type": "set_color", "key": key, "color": "@accent"})
        elif kind == "disappear":
            # Nothing to do here, but a plugin with real work should stop it:
            # nobody can see a key that is not on the current page.
            pass
        elif kind == "shutdown":
            return


if __name__ == "__main__":
    main()
