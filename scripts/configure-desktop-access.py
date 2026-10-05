#!/usr/bin/env python3
"""Allow selected data-volume UUIDs to automount for one active local desktop user.

No service restart, mount operation, fingerprint enrollment or desktop deployment is performed.
"""
import argparse
import json
import os
from pathlib import Path
import pwd
import re
import tempfile


def configurations(user, uuids):
    config = {
        "program_options": {"automount": True, "tray": False, "notify": False,
                            "file_manager": False, "password_prompt": False},
        "device_config": [{"id_uuid": uuid, "ignore": False, "automount": True} for uuid in uuids],
    }
    # UDisks' documented id.uuid detail, not a mutable /dev/sdX name or volume label.
    # Do not grant unlock, format, other-seat or other-user operations.
    rule = """// Managed by TemplarOS configure-desktop-access.py; explicit data-volume opt-in.
polkit.addRule(function(action, subject) {
    var uuids = %s;
    if (subject.user === %s && subject.local && subject.active &&
        (action.id === "org.freedesktop.udisks2.filesystem-mount" ||
         action.id === "org.freedesktop.udisks2.filesystem-mount-system") &&
        uuids.indexOf(action.lookup("id.uuid")) !== -1) {
        return polkit.Result.YES;
    }
});
""" % (json.dumps(uuids), json.dumps(user))
    return {"etc/templaros/udiskie.json": json.dumps(config, indent=2) + "\n",
            "etc/polkit-1/rules.d/49-templaros-data-volumes.rules": rule}


def atomic_write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".templaros-", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            os.fchmod(stream.fileno(), 0o644)
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--user", required=True, help="local desktop account")
    parser.add_argument("--mount-uuid", action="append", required=True,
                        help="explicit internal data filesystem UUID; repeat for each volume")
    parser.add_argument("--root", type=Path, default=Path("/"), help="offline root or isolated smoke root")
    parser.add_argument("--dry-run", action="store_true")
    args = parser.parse_args()
    if not re.fullmatch(r"[a-zA-Z_][a-zA-Z0-9_.@-]{0,255}", args.user):
        parser.error("invalid local username")
    uuids = sorted(set(args.mount_uuid))
    if any(not re.fullmatch(r"[a-zA-Z0-9-]{1,128}", uuid) for uuid in uuids):
        parser.error("UUID must contain only letters, digits and hyphens")
    root = args.root.resolve()
    if root == Path("/"):
        try:
            pwd.getpwnam(args.user)
        except KeyError:
            parser.error("desktop account does not exist")
        if not args.dry_run and os.geteuid() != 0:
            parser.error("installing system policy requires sudo; use --dry-run to review")
    for relative, content in configurations(args.user, uuids).items():
        path = root / relative
        if args.dry_run:
            print(f"# {path}\n{content}")
        else:
            atomic_write(path, content)
            print(path)
    if not args.dry_run:
        print("Configured selected data volumes. New login starts the polkit agent and automounter; no live session was changed.")


if __name__ == "__main__":
    main()
