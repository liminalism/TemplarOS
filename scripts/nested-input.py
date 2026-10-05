#!/usr/bin/env python3
"""Private nested-compositor commands; never synthesizes host/X11/uinput events."""
import argparse
import json
from pathlib import Path
import socket
import tempfile


class Injector:
    def __init__(self, path):
        self.directory = tempfile.TemporaryDirectory(prefix="templar-input-")
        self.socket = socket.socket(socket.AF_UNIX, socket.SOCK_DGRAM)
        self.socket.bind(str(Path(self.directory.name) / "client.sock"))
        self.socket.settimeout(5)
        self.path = str(path)

    def command(self, *words):
        self.socket.sendto(" ".join(map(str, words)).encode(), self.path)
        result = json.loads(self.socket.recv(65536))
        if "error" in result:
            raise RuntimeError(result["error"])
        return result

    def tap(self, code):
        self.command("key", code, "down")
        self.command("key", code, "up")

    def chord(self, modifiers, code):
        try:
            for modifier in modifiers:
                self.command("key", modifier, "down")
            self.tap(code)
        finally:
            for modifier in reversed(modifiers):
                self.command("key", modifier, "up")

    def type(self, text):
        # US keymap, matching the nested fixture's default keymap. Reject unsupported characters
        # before sending anything, so a partial shell command is never left in a test terminal.
        codes = dict(zip("1234567890", range(2, 12)))
        codes.update(zip("qwertyuiop", range(16, 26)))
        codes.update(zip("asdfghjkl", range(30, 39)))
        codes.update(zip("zxcvbnm", range(44, 51)))
        codes.update({" ": 57, "\n": 28, "\t": 15, "-": 12, "=": 13, "[": 26, "]": 27, ";": 39, "'": 40, "`": 41, "\\": 43, ",": 51, ".": 52, "/": 53})
        shifted = dict(zip("!@#$%^&*()_+{}:\"~|<>?", "1234567890-=[];'`\\,./"))
        events = []
        for character in text:
            base = shifted.get(character, character.lower())
            if base not in codes:
                raise ValueError(f"unsupported US-keymap character: {character!r}")
            events.append((codes[base], character != base))
        for code, shift in events:
            self.chord([42], code) if shift else self.tap(code)

    def close(self):
        self.socket.close()
        self.directory.cleanup()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    parser.add_argument("command", choices=["snapshot", "key", "button", "motion", "click", "type"])
    parser.add_argument("args", nargs="*")
    args = parser.parse_args()
    injector = Injector(args.socket)
    try:
        if args.command == "click":
            if len(args.args) != 2:
                parser.error("click requires X Y")
            injector.command("motion", *args.args)
            injector.command("button", 272, "down")
            result = injector.command("button", 272, "up")
        elif args.command == "type":
            if len(args.args) != 1:
                parser.error("type requires one text argument")
            injector.type(args.args[0])
            result = {"ok": True}
        else:
            result = injector.command(args.command, *args.args)
        print(json.dumps(result))
    finally:
        injector.close()


if __name__ == "__main__":
    main()
