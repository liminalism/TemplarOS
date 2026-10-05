#!/usr/bin/env python3
"""Actual Alt+Tab release commit and Home card click restore on an isolated compositor."""
import argparse
from pathlib import Path
import sys
import time
sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "scripts"))
from importlib import import_module
Injector = import_module("nested-input").Injector


def until(injector, predicate, description):
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        snapshot = injector.command("snapshot")
        if predicate(snapshot):
            return snapshot
        time.sleep(0.05)
    raise AssertionError(f"timed out waiting for {description}: {snapshot}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--socket", required=True)
    args = parser.parse_args()
    injector = Injector(args.socket)
    try:
        initial = until(injector, lambda s: len(s["windows"]) >= 2 and any(w["active"] for w in s["windows"]), "two mapped clients")
        old = next(w["id"] for w in initial["windows"] if w["active"])
        injector.command("key", 56, "down")  # Alt held: no activation while walking.
        injector.tap(15)
        walking = until(injector, lambda s: s["selected"] is not None and s["selected"] != old, "switcher selection")
        assert next(w["id"] for w in walking["windows"] if w["active"]) == old, walking
        selected = walking["selected"]
        injector.command("key", 56, "up")
        committed = until(injector, lambda s: s["selected"] is None and any(w["id"] == selected and w["active"] for w in s["windows"]), "release activation")
        assert not committed["home"], committed
        injector.chord([29, 56], 25)  # Ctrl+Alt+P: real park-focused input path.
        until(injector, lambda s: any(w["id"] == selected and w["parked"] for w in s["windows"]), "parked client")
        injector.chord([125], 32)  # Super+D: reveal actual shell Home surface.
        home = until(injector, lambda s: s["home"] and any(w["id"] == selected and w["card"] for w in s["windows"]), "published Home card target")
        card = next(w["card"] for w in home["windows"] if w["id"] == selected)
        injector.command("motion", card["x"] + card["width"] / 2, card["y"] + card["height"] / 2)
        injector.command("button", 272, "down")
        injector.command("button", 272, "up")
        restored = until(injector, lambda s: not s["home"] and any(w["id"] == selected and w["active"] and not w["parked"] for w in s["windows"]), "card-click unshelve and activate")
        print(f"nested input: ok (release commits {old} -> {selected}; shell Home card click restores {selected})")
    finally:
        # Even failed assertions must not strand held modifiers in the nested session.
        injector.command("key", 56, "up")
        injector.close()


if __name__ == "__main__":
    main()
