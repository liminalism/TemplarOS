#!/usr/bin/python3
"""hostd-only root-paired password writer. Never invoke with a password argument.

The caller supplies a sealed memfd as stdin. Preparation runs chpasswd in a
throwaway snapshot. The persistent intent contains encrypted shadow files only;
prepared intents roll back, committed intents roll forward before APIs resume.
"""
import argparse
import base64
import ctypes
import fcntl
import json
import os
import resource
import re
from pathlib import Path
import shutil
import signal
import stat
import subprocess
import tempfile
import time
import tomllib
import uuid

SCHEMA = 1
ACCOUNT_FILES = ("passwd", "shadow", "group", "gshadow")
LIBC = ctypes.CDLL(None, use_errno=True)


def run(*args, input=None, capture=False):
    parent = os.getpid()
    # Single-threaded helper: request death with this exact owned parent for
    # native children. Command timeouts also kill/wait their exact owned PID.
    def parent_death():
        if LIBC.prctl(1, signal.SIGKILL, 0, 0, 0) != 0 or os.getppid() != parent:
            os._exit(1)
    result = subprocess.run(args, input=input, stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, timeout=120, check=False, preexec_fn=parent_death)
    if result.returncode:
        raise RuntimeError("account maintenance command failed: " + args[0])
    return result.stdout if capture else None


def sync_dir(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def atomic(path, data, mode=0o600, uid=0, gid=0, attributes=None):
    fd, name = tempfile.mkstemp(prefix=".templaros-account-", dir=path.parent)
    try:
        os.fchmod(fd, mode)
        os.fchown(fd, uid, gid)
        for key, value in (attributes or {}).items():
            os.setxattr(fd, key, base64.b64decode(value, validate=True))
        with os.fdopen(fd, "wb") as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(name, path)
        sync_dir(path.parent)
    finally:
        if os.path.exists(name):
            os.unlink(name)


def save_intent(path, intent):
    atomic(path, json.dumps(intent, sort_keys=True).encode())


def is_mountpoint(path):
    # st_dev/st_ino (os.path.ismount) cannot recognize same-filesystem State binds.
    target = os.fsencode(os.path.realpath(path))
    with open("/proc/self/mountinfo", "rb") as stream:
        for line in stream:
            fields = line.split()
            if len(fields) > 4 and re.sub(rb"\\([0-7]{3})", lambda match: bytes([int(match[1], 8)]), fields[4]) == target:
                return True
    return False


def establish_gate(root):
    directory = root / "var/lib/templaros/account-maintenance"
    if (root / "usr/share/templaros/layout.toml").exists() and not is_mountpoint(directory):
        raise RuntimeError("native account gate State bind is not mounted; offline layout repair required")
    directory.mkdir(mode=0o755, parents=True, exist_ok=True)
    for path in (directory, *directory.parents):
        info = path.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022 or info.st_mode & 0o111 != 0o111:
            raise RuntimeError("untrusted/unsearchable account gate directory; review account recovery service")
        if path == root:
            break
    marker = directory / "active.json"
    try:
        info = marker.lstat()
    except FileNotFoundError:
        info = None
    if info is not None:
        if not stat.S_ISREG(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o022 or info.st_size > 1024:
            raise RuntimeError("malformed account gate; review account recovery service")
        value = json.loads(marker.read_text())
        if (not isinstance(value, dict) or set(value) != {"schema", "maintenance"} or
                type(value["schema"]) is not int or value["schema"] != SCHEMA or value["maintenance"] is not True):
            raise RuntimeError("unsupported account gate; review account recovery service")
    else:
        atomic(marker, json.dumps({"schema": SCHEMA, "maintenance": True}).encode(), 0o644)
    return marker


def clear_gate(marker):
    marker.unlink()
    sync_dir(marker.parent)


def safe_subvolume(value):
    return value.startswith("@") and all(part not in ("", ".", "..") and
        all(character.isascii() and (character.isalnum() or character in "@._-") for character in part)
        for part in value.split("/"))


def account_data(root):
    result = {}
    for name in ACCOUNT_FILES:
        path = root / "etc" / name
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or info.st_uid != 0:
            raise RuntimeError("account database is not a root-owned regular file")
        if name in ("shadow", "gshadow") and info.st_mode & 0o027:
            raise RuntimeError("encrypted account database has unsafe permissions")
        result[name] = path.read_bytes()
    # A journal must never accidentally preserve a cleartext shadow password.
    for name in ("shadow", "gshadow"):
        for line in result[name].decode("utf-8").splitlines():
            fields = line.split(":")
            if len(fields) < 2 or (fields[1] and not fields[1].startswith(("$", "!", "*"))):
                raise RuntimeError("unsupported account password representation")
    return result


def root_identity(value, template):
    prefix = template.removesuffix("{id}")
    return value == "@" or (isinstance(value, str) and value.startswith(prefix) and
        re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]{0,63}", value[len(prefix):]) is not None)


def validate_intent(intent, template):
    if (not isinstance(intent, dict) or type(intent.get("schema")) is not int or intent["schema"] != SCHEMA or
            intent.get("phase") not in ("preparing", "prepared", "committed") or not isinstance(intent.get("roots"), list)):
        raise RuntimeError("unsupported account journal; refusing downgrade or guessing recovery")
    scratch = intent.get("scratch")
    prefix = template.removesuffix("{id}") + ".accounts-"
    if not isinstance(scratch, str) or not scratch.startswith(prefix) or not re.fullmatch(r"[a-f0-9]{32}", scratch[len(prefix):]):
        raise RuntimeError("account scratch is outside its exact owned namespace")
    if (intent["phase"] == "preparing") != (len(intent["roots"]) == 0):
        raise RuntimeError("malformed account journal root set")
    identities = set()
    for entry in intent["roots"]:
        if (not isinstance(entry, dict) or not root_identity(entry.get("subvolume"), template) or
                type(entry.get("read_only")) is not bool or any(type(entry.get(key)) is not int for key in ("mode", "uid", "gid")) or
                not 0 <= entry["mode"] <= 0o777 or entry["mode"] & 0o027 or entry["uid"] != 0 or not 0 <= entry["gid"] <= 0xffffffff):
            raise RuntimeError("malformed account recovery metadata")
        if entry["subvolume"] in identities:
            raise RuntimeError("duplicate account recovery root")
        identities.add(entry["subvolume"])
        for key in ("old_shadow", "new_shadow"):
            data = base64.b64decode(entry[key], validate=True)
            for line in data.decode("utf-8").splitlines():
                fields = line.split(":")
                if len(fields) != 9 or (fields[1] and not fields[1].startswith(("$", "!", "*"))):
                    raise RuntimeError("journal contains an unsupported encrypted shadow representation")
        attributes = entry.get("attributes", {})
        if not isinstance(attributes, dict) or any(not isinstance(name, str) for name in attributes):
            raise RuntimeError("malformed shadow extended attributes")
        for value in attributes.values():
            base64.b64decode(value, validate=True)


def check_reader(root):
    declaration = tomllib.loads((root / "usr/share/templaros/state-compat.toml").read_text())
    if (any(type(declaration.get(key)) is not int for key in ("minimum", "maximum", "account_writer")) or
            not (declaration["minimum"] <= SCHEMA <= declaration["maximum"] and declaration["account_writer"] == SCHEMA)):
        raise RuntimeError("deployment account/state reader is incompatible")


def mount_root(device, subvolume, destination):
    destination.mkdir()
    run("mount", "-t", "btrfs", "-o", "ro,subvol=" + subvolume, device, str(destination))


def property_read(root):
    text = run("btrfs", "property", "get", "-ts", str(root), "ro", capture=True).decode().strip()
    if text not in ("ro=true", "ro=false"):
        raise RuntimeError("cannot determine subvolume read-only property")
    return text == "ro=true"


def writable(root, original_ro, property_root):
    if original_ro:
        run("btrfs", "property", "set", "-ts", str(property_root), "ro", "false")
    run("mount", "-o", "remount,bind,rw", str(root))


def restore_ro(root, original_ro, property_root):
    run("sync", "-f", str(root))
    if original_ro:
        run("mount", "-o", "remount,bind,ro", str(root))
        run("btrfs", "property", "set", "-ts", str(property_root), "ro", "true")


def live_identity(root):
    report = json.loads(run("findmnt", "--json", "--target", str(root), "--output", "SOURCE,FSTYPE,FSROOT,OPTIONS,UUID", capture=True))
    rows = report.get("filesystems", [])
    if len(rows) != 1 or rows[0]["fstype"] != "btrfs":
        raise RuntimeError("account maintenance requires the native Btrfs root")
    row = rows[0]
    source = row["source"].split("[", 1)[0]
    identity = row["fsroot"].lstrip("/")
    if not source.startswith("/dev/") or not safe_subvolume(identity):
        raise RuntimeError("unsupported root identity")
    if not isinstance(row.get("uuid"), str) or not re.fullmatch(r"[a-fA-F0-9-]{36}", row["uuid"]):
        raise RuntimeError("native root lacks its actual filesystem UUID")
    return source, identity, row["options"].split(","), row.get("uuid")

def read_secret():
    required = fcntl.F_SEAL_WRITE | fcntl.F_SEAL_GROW | fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_SEAL
    if fcntl.fcntl(0, fcntl.F_GET_SEALS) & required != required:
        raise RuntimeError("password descriptor must be an immutable sealed memfd")
    size = os.fstat(0).st_size
    if not 1 <= size <= 1024:
        raise RuntimeError("password is outside the 1..1024 byte bound")
    data = os.pread(0, size, 0)
    try:
        data.decode("utf-8")
    except UnicodeDecodeError:
        raise RuntimeError("password must be valid UTF-8") from None
    if len(data) != size or any(character in data for character in (0, 10, 13)):
        raise RuntimeError("password contains an unsupported delimiter")
    return bytearray(data)


def recover(intent_path, intent, top, work, device, template):
    validate_intent(intent, template)
    targets = []
    locks = []
    restored = set()
    try:
        for index, entry in enumerate(intent["roots"]):
            if not safe_subvolume(entry["subvolume"]):
                raise RuntimeError("unsafe account recovery subvolume")
            root = work / ("recover-" + str(index))
            mount_root(device, entry["subvolume"], root)
            targets.append((root, entry))
        # Match shadow-utils' POSIX lock before comparing/replacing any root.
        # Recovery can run while existing sessions still exist on default-RW roots.
        for root, entry in targets:
            writable(root, entry["read_only"], top / entry["subvolume"])
            handle = (root / "etc/.pwd.lock").open("a+b")
            locks.append(handle)
            fcntl.lockf(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
        for root, entry in targets:
            old = base64.b64decode(entry["old_shadow"], validate=True)
            new = base64.b64decode(entry["new_shadow"], validate=True)
            current = (root / "etc/shadow").read_bytes()
            if current not in (old, new):
                # Never roll a concurrent/default-RW password change backwards.
                raise RuntimeError("account recovery found unjournaled credentials; reviewed recovery required")
        for root, entry in targets:
            shadow = base64.b64decode(entry["new_shadow" if intent["phase"] == "committed" else "old_shadow"], validate=True)
            if (root / "etc/shadow").read_bytes() != shadow:
                atomic(root / "etc/shadow", shadow, entry["mode"], entry["uid"], entry["gid"], entry.get("attributes"))
        for handle, (_, entry) in zip(locks, targets):
            if entry["read_only"]:
                handle.close()
        for root, entry in targets:
            restore_ro(root, entry["read_only"], top / entry["subvolume"])
            restored.add(root)
        scratch_id = intent.get("scratch")
        if scratch_id:
            if not safe_subvolume(scratch_id):
                raise RuntimeError("unsafe account recovery scratch")
            scratch = top / scratch_id
            if scratch.exists():
                run("btrfs", "subvolume", "delete", str(scratch))
        intent_path.unlink()
        sync_dir(intent_path.parent)
    finally:
        for handle in locks:
            handle.close()
        cleanup_errors = []
        for root, entry in reversed(targets):
            if root not in restored:
                try:
                    restore_ro(root, entry["read_only"], top / entry["subvolume"])
                except Exception as error:
                    cleanup_errors.append(error)
            try:
                run("umount", str(root))
            except Exception as error:
                cleanup_errors.append(error)
        if cleanup_errors:
            raise RuntimeError("account recovery could not restore its owned roots; journal/gate require reviewed recovery") from cleanup_errors[0]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", default="/")
    parser.add_argument("--state", required=True)
    parser.add_argument("--roots", help="root-owned root-identity request JSON")
    parser.add_argument("--uid", type=int)
    parser.add_argument("--recover", action="store_true")
    args = parser.parse_args()
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    if os.geteuid() != 0:
        raise RuntimeError("account maintenance is hostd-owned")
    root = Path(args.root)
    state = Path(args.state)
    state.mkdir(mode=0o700, parents=True, exist_ok=True)
    info = state.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != 0 or info.st_mode & 0o077:
        raise RuntimeError("account journal directory must be private to root")
    with (state / "lock").open("a+b") as lock:
        os.chmod(state / "lock", 0o600)
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        gate = establish_gate(root)
        intent_path = state / "intent.json"
        # Holding the writer lock and finding no durable intent proves that no
        # credential mutation began. Only this case may clear an orphan marker.
        if args.recover and not intent_path.exists():
            clear_gate(gate)
            return
        installed = tomllib.loads((root / "etc/templaros/install.toml").read_text())
        template = tomllib.loads((root / "usr/share/templaros/layout.toml").read_text())["deployment"]["subvolume_template"]
        if template.count("{id}") != 1 or not template.endswith("/{id}") or not safe_subvolume(template.replace("{id}", "candidate")):
            raise RuntimeError("unsafe account root layout")
        device, live_subvolume, original_options, filesystem_uuid = live_identity(root)
        if installed.get("root_uuid") != filesystem_uuid:
            raise RuntimeError("account maintenance root UUID differs from install identity")
        if type(installed.get("read_only", False)) is not bool:
            raise RuntimeError("malformed native root read-only policy")
        if installed.get("read_only", False) and "ro" not in original_options:
            raise RuntimeError("sealed installation live root is not read-only")
        work = Path(tempfile.mkdtemp(prefix="templaros-accounts-", dir="/run"))
        top = work / "top"
        top.mkdir()
        run("mount", "-t", "btrfs", "-o", "rw,subvolid=5", device, str(top))
        mounted = []
        scratch = None
        intent = None
        try:
            if intent_path.exists():
                recover(intent_path, json.loads(intent_path.read_text()), top, work, device, template)
            if args.recover:
                return
            if args.uid is None or args.uid < 1000 or not args.roots:
                raise RuntimeError("only an authenticated local user's own password may change")
            request_path = Path(args.roots)
            request_info = request_path.lstat()
            if not stat.S_ISREG(request_info.st_mode) or request_info.st_uid != 0 or request_info.st_mode & 0o077:
                raise RuntimeError("account root request must be private to root")
            request = json.loads(request_path.read_text())
            if type(request.get("schema")) is not int or request["schema"] != SCHEMA:
                raise RuntimeError("unsupported account root request")
            identities = set(request["subvolumes"])
            identities.add(live_subvolume)
            if not identities or any(not root_identity(identity, template) for identity in identities):
                raise RuntimeError("unsafe account root set")
            live_accounts = account_data(root)
            users = [line.split(":") for line in live_accounts["passwd"].decode().splitlines()]
            matching = [user for user in users if len(user) == 7 and user[2] == str(args.uid)]
            if len(matching) != 1:
                raise RuntimeError("caller does not name exactly one local account")
            username = matching[0][0]
            if not username or any(not (character.isascii() and (character.isalnum() or character in "_-.")) for character in username):
                raise RuntimeError("unsupported local account name")
            # Preflight every root before any credential or read-only-property mutation.
            entries = []
            for index, identity in enumerate(sorted(identities)):
                target = work / ("root-" + str(index))
                mount_root(device, identity, target)
                mounted.append(target)
                check_reader(target)
                if account_data(target) != live_accounts:
                    raise RuntimeError("root-paired accounts differ; reviewed account migration is required before password update")
                info = (target / "etc/shadow").stat()
                entries.append({"subvolume": identity, "read_only": property_read(target),
                    "mode": stat.S_IMODE(info.st_mode), "uid": info.st_uid, "gid": info.st_gid,
                    "old_shadow": base64.b64encode(live_accounts["shadow"]).decode()})
                entries[-1]["attributes"] = {name: base64.b64encode(os.getxattr(target / "etc/shadow", name, follow_symlinks=False)).decode()
                    for name in os.listxattr(target / "etc/shadow", follow_symlinks=False)}
            check_reader(root)
            # Hash and apply PAM's real password policy in a disposable source snapshot.
            nonce = uuid.uuid4().hex
            scratch_id = template.replace("{id}", ".accounts-" + nonce)
            if not safe_subvolume(scratch_id):
                raise RuntimeError("unsafe account scratch layout")
            scratch = top / scratch_id
            # Durable ownership precedes scratch creation. A crash during PAM hashing
            # leaves only a journal-owned, non-bootable scratch to remove on startup.
            intent = {"schema": SCHEMA, "phase": "preparing", "scratch": scratch_id, "roots": []}
            save_intent(intent_path, intent)
            run("btrfs", "subvolume", "snapshot", str(top / live_subvolume), str(scratch))
            # The scratch snapshot is private, writable and never selectable for boot.
            (scratch / "dev").mkdir(exist_ok=True)
            run("mount", "--rbind", "/dev", str(scratch / "dev"))
            run("mount", "--make-rprivate", str(scratch / "dev"))
            secret = read_secret()
            payload = bytearray(username.encode() + b":" + secret + b"\n")
            try:
                run("chroot", str(scratch), "/usr/sbin/chpasswd", input=payload)
            finally:
                secret[:] = b"\0" * len(secret)
                payload[:] = b"\0" * len(payload)
                run("umount", "-R", str(scratch / "dev"))
            changed = account_data(scratch)
            for name in ("passwd", "group", "gshadow"):
                if changed[name] != live_accounts[name]:
                    raise RuntimeError("password policy changed account structure; refusing root propagation")
            old_lines = live_accounts["shadow"].decode().splitlines()
            new_lines = changed["shadow"].decode().splitlines()
            if len(old_lines) != len(new_lines):
                raise RuntimeError("password policy changed account structure")
            affected = 0
            for old, new in zip(old_lines, new_lines):
                if old != new:
                    before, after = old.split(":"), new.split(":")
                    if len(before) != 9 or len(after) != 9 or before[0] != username or after[0] != username or before[3:] != after[3:]:
                        raise RuntimeError("password policy changed unrelated shadow fields")
                    affected += 1
            if affected != 1:
                raise RuntimeError("password policy did not change exactly the caller's password")
            for entry in entries:
                entry["new_shadow"] = base64.b64encode(changed["shadow"]).decode()
            intent = {"schema": SCHEMA, "phase": "prepared", "scratch": scratch_id, "roots": entries}
            save_intent(intent_path, intent)
            locks = []
            try:
                for target, entry in zip(mounted, entries):
                    writable(target, entry["read_only"], top / entry["subvolume"])
                    handle = (target / "etc/.pwd.lock").open("a+b")
                    fcntl.lockf(handle, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    locks.append(handle)
                    if account_data(target) != live_accounts:
                        raise RuntimeError("account database changed during maintenance preflight")
                for target, entry in zip(mounted, entries):
                    atomic(target / "etc/shadow", changed["shadow"], entry["mode"], entry["uid"], entry["gid"], entry["attributes"])
                for handle, entry in zip(locks, entries):
                    if entry["read_only"]:
                        handle.close()
                for target, entry in zip(mounted, entries):
                    restore_ro(target, entry["read_only"], top / entry["subvolume"])
                _, still_live, options, _ = live_identity(root)
                if still_live != live_subvolume or ("ro" in original_options and "ro" not in options):
                    raise RuntimeError("live root mount identity/read-only invariant changed")
                intent["phase"] = "committed"
                save_intent(intent_path, intent)
            finally:
                for handle in locks:
                    handle.close()
            for target in reversed(mounted):
                run("umount", str(target))
            mounted.clear()
            recover(intent_path, intent, top, work, device, template)
            scratch = None
        except BaseException:
            for target in reversed(mounted):
                run("umount", str(target))
            mounted.clear()
            if intent_path.exists():
                recover(intent_path, json.loads(intent_path.read_text()), top, work, device, template)
                scratch = None
            raise
        finally:
            if scratch is not None and scratch.exists():
                # A crash before intent persistence leaves only a non-bootable scratch.
                # No credential/root-property mutation occurred before that point.
                dev = scratch / "dev"
                if os.path.ismount(dev):
                    run("umount", "-R", str(dev))
                run("btrfs", "subvolume", "delete", str(scratch))
            run("umount", str(top))
            shutil.rmtree(work)
            if not intent_path.exists():
                clear_gate(gate)


if __name__ == "__main__":
    try:
        main()
    except BaseException as error:
        # Subprocess stderr and password data are deliberately never rendered.
        print("account maintenance refused or recovered: " + str(error), file=__import__("sys").stderr)
        raise SystemExit(1)
