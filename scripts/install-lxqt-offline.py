#!/usr/bin/env python3
"""Install the verified LXQt backport into an offline TemplarOS root.

Keeps a Btrfs root/state snapshot pair, rejects package removals, and inhibits
service startup in a private PID/mount namespace with no live desktop buses.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import posixpath
import shutil
import subprocess
import sys


def run(argv, **kwargs):
    print("+", " ".join(map(str, argv)), flush=True)
    return subprocess.run(list(map(str, argv)), check=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--debs", type=Path, required=True)
    parser.add_argument("--namespace", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    root, state, debdir = (path.resolve(strict=True) for path in (args.root, args.state, args.debs))
    if root.samefile("/") or state.samefile("/var"):
        parser.error("only an offline installation is accepted")
    if "VERSION_CODENAME=resolute" not in (root / "etc/os-release").read_text():
        parser.error("the target must be Ubuntu Resolute")
    if not (root / "etc/templaros/install.toml").exists():
        parser.error("the target must be installed TemplarOS")
    result = json.loads((debdir.parent / "build-result.json").read_text())
    if result["release"] != "2.4.0" or result["target"] != "resolute":
        parser.error("unexpected backport release or target")
    selected = []
    expected = {}
    for entry in result["packages"]:
        package = entry["package"]
        if package.endswith(("-dev", "-dbgsym", "-dbg")) or package == "lxqt-build-tools":
            continue
        file = debdir / entry["file"]
        if hashlib.sha256(file.read_bytes()).hexdigest() != entry["sha256"]:
            parser.error(f"package hash mismatch: {file}")
        if package in expected:
            parser.error(f"multiple versions of {package}")
        selected.append(file)
        expected[package] = entry["version"]
    if not {"templaros-greeter", "templaros-lxqt", "lxqt-session", "lxqt-panel", "pcmanfm-qt"} <= expected.keys():
        parser.error("the complete desktop and updated greeter must be present")
    if expected["templaros-greeter"] != expected["templaros-lxqt"]:
        parser.error("greeter and integration package build identities differ")
    if not args.namespace:
        return run(["pkexec", "/usr/bin/unshare", "--mount", "--propagation", "private", "--pid", "--fork",
                    "/usr/bin/python3", __file__, *sys.argv[1:], "--namespace"])
    if os.geteuid() != 0:
        parser.error("offline installation requires root")
    run(["mount", "--bind", state, root / "var"])
    for name in ("run", "tmp", "dev", "sys"):
        run(["mount", "-t", "tmpfs", "tmpfs", root / name])
    (root / "tmp").chmod(0o1777)
    (root / "dev/pts").mkdir()
    run(["mount", "-t", "devpts", "devpts", "-o", "newinstance,ptmxmode=0666,mode=0620", root / "dev/pts"])
    (root / "dev/ptmx").symlink_to("pts/ptmx")
    (root / "dev/fd").symlink_to("/proc/self/fd")
    for name in ("null", "zero", "random", "urandom"):
        (root / "dev" / name).touch()
        run(["mount", "--bind", Path("/dev") / name, root / "dev" / name])
    run(["mount", "-t", "proc", "proc", root / "proc"])
    resolv = root / "etc/resolv.conf"
    if resolv.is_symlink():
        link = str(resolv.readlink())
        destination = posixpath.normpath(link if link.startswith("/") else "/etc/" + link)
        if not destination.startswith("/run/"):
            raise ValueError("unsupported offline resolver symlink")
        private_resolv = root / destination.lstrip("/")
        private_resolv.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile("/etc/resolv.conf", private_resolv)
    (root / "tmp/lxqt-debs").mkdir()
    run(["mount", "--bind", debdir, root / "tmp/lxqt-debs"])
    run(["mount", "-o", "remount,bind,ro", root / "tmp/lxqt-debs"])
    environment = dict(os.environ, DEBIAN_FRONTEND="noninteractive", SYSTEMD_OFFLINE="1")
    def inside(argv, **kwargs):
        return run(["chroot", root, *argv], env=environment, **kwargs)
    arguments = ["apt-get", "--no-remove", "--no-install-recommends", "install",
                 *[f"/tmp/lxqt-debs/{file.name}" for file in selected]]
    with (debdir.parent / "install-plan.log").open("w") as log:
        inside([*arguments[:1], "--simulate", *arguments[1:]], stdout=log, stderr=subprocess.STDOUT)
    snapshots = []
    snapshot_dir = root.parent / "@snapshots"
    if not snapshot_dir.is_dir():
        raise ValueError("the installed Btrfs snapshot directory is absent")
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    for label, source in (("root", root), ("state", state)):
        snapshot = snapshot_dir / f"lxqt-before-{stamp}-{label}"
        run(["btrfs", "subvolume", "snapshot", "-r", source, snapshot])
        snapshots.append(str(snapshot))
    policy = root / "usr/sbin/policy-rc.d"
    existed = policy.exists()
    if not existed:
        policy.touch()
    private_policy = root / "tmp/policy-rc.d"
    private_policy.write_text("#!/bin/sh\nexit 101\n")
    private_policy.chmod(0o755)
    run(["mount", "--bind", private_policy, policy])
    try:
        inside([*arguments[:1], "-y", *arguments[1:]])
        audit = inside(["dpkg", "--audit"], capture_output=True, text=True)
        if audit.stdout.strip():
            raise ValueError(audit.stdout)
        installed = {}
        for package, version in expected.items():
            found = inside(["dpkg-query", "-W", "-f=${Version}", package], capture_output=True, text=True).stdout
            if found != version:
                raise ValueError(f"{package}: expected {version}, installed {found}")
            installed[package] = found
        links = inside(["ldd", "/usr/bin/lxqt-session", "/usr/bin/lxqt-panel",
                        "/usr/bin/pcmanfm-qt", "/usr/bin/templar-greeter"], capture_output=True, text=True)
        if "not found" in links.stdout:
            raise ValueError(links.stdout)
        if not (root / "usr/libexec/templar-x11-session").stat().st_mode & 0o111:
            raise ValueError("the X11 launcher is not executable")
        observed = {"root": str(root), "state": str(state), "snapshots": snapshots,
                    "installed": installed, "dpkg_audit": audit.stdout,
                    "x11_session": (root / "usr/share/xsessions/lxqt.desktop").read_text(),
                    "window_manager_config": (root / "etc/xdg/lxqt/session.conf").read_text(),
                    "appearance_config": (root / "etc/xdg/lxqt/lxqt.conf").read_text(),
                    "physical_login_tested": False}
        path = debdir.parent / "install-result.json"
        path.write_text(json.dumps(observed, indent=2) + "\n")
        uid = int(os.environ["PKEXEC_UID"])
        os.chown(path, uid, -1)
        print("Offline LXQt installation and package/link checks passed", flush=True)
    finally:
        run(["umount", policy])
        if not existed:
            policy.unlink()


if __name__ == "__main__":
    main()
