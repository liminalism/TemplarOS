#!/usr/bin/python3 -I
"""Account-only PAM gate; no password, user identity, journal, or secret access."""
import json
import os
from pathlib import Path
import re
import stat
import sys

DIRECTORY = Path("/var/lib/templaros/account-maintenance")
MARKER = DIRECTORY / "active.json"


def is_mountpoint(path):
    # Same-filesystem State binds do not change st_dev and are real mountpoints.
    target = os.fsencode(os.path.realpath(path))
    with open("/proc/self/mountinfo", "rb") as stream:
        for line in stream:
            fields = line.split()
            if len(fields) > 4 and re.sub(rb"\\([0-7]{3})", lambda match: bytes([int(match[1], 8)]), fields[4]) == target:
                return True
    return False


def check():
    # This is readable for every PAM caller, including a real-UID user invoking su
    # or polkit. Authority is filesystem ownership, never the caller's UID.
    for path in (DIRECTORY, *DIRECTORY.parents):
        info = path.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022 or info.st_mode & 0o111 != 0o111:
            raise RuntimeError("untrusted or unsearchable account maintenance gate directory")
    if Path("/etc/templaros/install.toml").exists() and not is_mountpoint(DIRECTORY):
        raise RuntimeError("native account maintenance State bind is not mounted")
    try:
        info = MARKER.lstat()
    except FileNotFoundError:
        return
    if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022 or info.st_size > 1024:
        raise RuntimeError("malformed account maintenance gate")
    with MARKER.open("rb") as stream:
        value = json.load(stream)
    if (not isinstance(value, dict) or set(value) != {"schema", "maintenance"} or
            type(value["schema"]) is not int or value["schema"] != 1 or value["maintenance"] is not True):
        raise RuntimeError("unsupported account maintenance gate")
    raise RuntimeError("root-paired account maintenance is pending")


if __name__ == "__main__":
    try:
        check()
    except (OSError, ValueError, RuntimeError):
        print("TemplarOS account maintenance blocks login. An administrator must review/recover templar-account-recovery.service and the root-owned account maintenance journal before retrying.", file=sys.stderr)
        sys.exit(1)
