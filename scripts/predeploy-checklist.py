#!/usr/bin/env python3
"""Print unsatisfied manual acceptance from active AKR planning heads; never read its cache."""
import argparse
import concurrent.futures
import json
from pathlib import Path
import re
import subprocess

ROOT = Path(__file__).resolve().parent.parent


def akr(*arguments):
    result = subprocess.run(
        ["akr", "--format", "json", *arguments], cwd=ROOT,
        check=True, capture_output=True, text=True,
    )
    return json.loads(result.stdout)["result"]


def section(record, check):
    text = (record["key"] + " " + check["statement"]).lower()
    for name, pattern in [
        ("Login and session recovery", r"greeter|fingerprint|login|log in|lock screen|locks-a"),
        ("Installation and rollback", r"\bdeploy|\binstallation|\binstaller|\brollback|\bboot|\biso|\bmedium|\bhardware|\bsuspend"),
        ("Files and storage", r"file chooser|pcmanfm|archive|mount|drive|partition|storage"),
        ("Windows, Home and appearance", r"window|home|garden|card|rail|bar|motion|switch|chrome|widget"),
    ]:
        if re.search(pattern, text):
            return name
    return "Applications and integration"


def collect():
    # The authoritative active-work view enumerates heads independently of search ranking.
    text = akr("view", "active-work")["text"]
    keys = sorted(set(re.findall(r"@(\w+\.(?:work|milestone)\.[\w-]+(?:\.[\w-]+)*)(?:/\d+)?", text)))
    rows = []
    with concurrent.futures.ThreadPoolExecutor(max_workers=4) as pool:
        for record in pool.map(lambda key: akr("get", key), keys):
            if record["state"] != "active":
                continue
            for check in record.get("acceptance", []):
                if check["method"] != "manual" or check["verdict"].startswith("satisfied"):
                    continue
                rows.append({
                    "section": section(record, check),
                    "record": f"@{record['key']}/{record['rev']}",
                    "title": record["title"], "check": check["id"],
                    "try": check["statement"], "standing": check["verdict"],
                })
    return sorted(rows, key=lambda row: (row["section"], row["record"], row["check"]))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--json", action="store_true", help="emit structured checklist rows")
    args = parser.parse_args()
    rows = collect()
    if args.json:
        print(json.dumps(rows, indent=2, ensure_ascii=False))
        return
    print("# Generated pre-deploy live review checklist\n")
    print("Generated from active AKR heads. Outstanding checks are not claims of working behavior.\n")
    print("Install verified packages first; logout/login is the owner's action. Do not run deployment or session-kill commands from this checklist.\n")
    previous = None
    for row in rows:
        if row["section"] != previous:
            print(f"\n## {row['section']}\n")
            previous = row["section"]
        print(f"- [ ] {row['record']}#{row['check']} — {row['title']}\n  {row['try']}")
    print(f"\n{len(rows)} outstanding manual checks.")


if __name__ == "__main__":
    main()
