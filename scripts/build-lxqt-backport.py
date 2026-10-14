#!/usr/bin/env python3
"""Rebuild the pinned LXQt release with Resolute libraries in a disposable overlay.

Uses an offline mounted TemplarOS root as the build base. It never writes that
root, installs Stonking binaries, or connects to a running desktop's buses.
Source archives and Debian build outputs remain in --work for reproduction.
"""
import argparse
from email.utils import formatdate
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import urllib.request

REPO = Path(__file__).resolve().parent.parent


def run(argv, **kwargs):
    print("+", " ".join(map(str, argv)), flush=True)
    return subprocess.run(list(map(str, argv)), check=True, **kwargs)


def write_result(work, lock, uid, gid):
    for source in lock["sources"]:
        expected = source["version"] + "~templaros26.04.1"
        if (work / "sources" / source["name"] / "built-version").read_text().strip() != expected:
            raise ValueError(f"incomplete build: {source['name']}")
    result = {"release": lock["release"], "target": lock["target"], "packages": []}
    for deb in sorted((work / "debs").glob("*.deb")):
        fields = subprocess.check_output(["dpkg-deb", "-f", str(deb), "Package", "Version"], text=True)
        metadata = dict(line.split(": ", 1) for line in fields.splitlines())
        result["packages"].append({"file": deb.name, "package": metadata["Package"],
                                   "version": metadata["Version"],
                                   "sha256": hashlib.sha256(deb.read_bytes()).hexdigest()})
    path = work / "build-result.json"
    path.write_text(json.dumps(result, indent=2) + "\n")
    if os.geteuid() == 0:
        os.chown(path, uid, gid)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--work", type=Path, default=REPO / "target/lxqt-2.4")
    parser.add_argument("--jobs", type=int, default=8)
    parser.add_argument("--namespace", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    args.root = args.root.resolve(strict=True)
    args.state = args.state.resolve(strict=True)
    args.work = args.work.resolve()
    if args.root.samefile("/") or args.state.samefile("/var"):
        parser.error("the build base must be an offline installation")
    if 'VERSION_CODENAME=resolute' not in (args.root / "etc/os-release").read_text():
        parser.error("the build base must be Ubuntu Resolute")
    if not (args.root / "etc/templaros/install.toml").exists():
        parser.error("the build base must be an installed TemplarOS root")
    if not args.namespace:
        args.work.mkdir(parents=True, exist_ok=True)
        return run(["pkexec", "/usr/bin/unshare", "--mount", "--propagation", "private",
                    "/usr/bin/python3", __file__, *sys.argv[1:], "--namespace"])
    if os.geteuid() != 0:
        parser.error("namespace setup requires root")
    uid = int(os.environ["PKEXEC_UID"])
    gid = int(subprocess.check_output(["id", "-g", str(uid)], text=True))
    lock = json.loads((REPO / "distro/lxqt/backport.json").read_text())
    root = args.work / "buildroot"
    upper = args.work / "overlay-upper"
    overlay_work = args.work / "overlay-work"
    for path in (root, upper, overlay_work):
        path.mkdir(exist_ok=True)
    run(["mount", "-t", "overlay", "overlay", "-o",
         f"lowerdir={args.root},upperdir={upper},workdir={overlay_work}", root])
    # /var is a separate subvolume: copy only package-management state, never
    # the installed session's file indexes, logs, services or personal data.
    for name in ("lib/dpkg", "lib/apt", "cache/debconf", "cache/apt"):
        destination = root / "var" / name
        if not destination.exists():
            destination.parent.mkdir(parents=True, exist_ok=True)
            run(["cp", "-a", "--reflink=auto", args.state / name, destination])
    for name in ("run", "tmp", "dev"):
        run(["mount", "-t", "tmpfs", "tmpfs", root / name])
    (root / "tmp").chmod(0o1777)
    (root / "dev/pts").mkdir()
    run(["mount", "-t", "devpts", "devpts", "-o", "newinstance,ptmxmode=0666,mode=0620", root / "dev/pts"])
    (root / "dev/ptmx").symlink_to("pts/ptmx")
    (root / "dev/fd").symlink_to("/proc/self/fd")
    (root / "var/log/apt").mkdir(parents=True, exist_ok=True)
    for name in ("null", "zero", "random", "urandom"):
        (root / "dev" / name).touch()
        run(["mount", "--bind", Path("/dev") / name, root / "dev" / name])
    run(["mount", "-t", "proc", "proc", root / "proc"])
    run(["mount", "-t", "tmpfs", "tmpfs", root / "sys"])
    (root / "build").mkdir(exist_ok=True)
    run(["mount", "--bind", args.work, root / "build"])
    # No service postinst may contact the host or start a daemon in this build.
    policy = root / "usr/sbin/policy-rc.d"
    policy.write_text("#!/bin/sh\nexit 101\n")
    policy.chmod(0o755)
    resolv = root / "etc/resolv.conf"
    if resolv.is_symlink():
        resolv.unlink()
    shutil.copyfile("/etc/resolv.conf", resolv)
    sources = root / "etc/apt/sources.list.d"
    for path in sources.iterdir():
        if path.suffix in (".list", ".sources"):
            path.unlink()
    (root / "etc/apt/sources.list").write_text("")
    (sources / "lxqt-build.sources").write_text(
        f"Types: deb\nURIs: {lock['binary_snapshot']}\n"
        "Suites: resolute resolute-updates resolute-security\n"
        "Components: main universe restricted multiverse\n"
        "Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n")
    def inside(argv, **kwargs):
        return run(["chroot", root, *argv], **kwargs)
    inside(["apt-get", "update"])
    environment = dict(os.environ, DEBIAN_FRONTEND="noninteractive",
                       DEB_BUILD_OPTIONS=f"parallel={args.jobs}", HOME="/build/home")
    (args.work / "home").mkdir(exist_ok=True)
    os.chown(args.work / "home", uid, gid)
    output = args.work / "debs"
    output.mkdir(exist_ok=True)
    os.chown(output, uid, gid)
    for source in lock["sources"]:
        name = source["name"]
        source_dir = args.work / "sources" / name
        source_dir.mkdir(parents=True, exist_ok=True)
        for file in source["files"]:
            path = source_dir / file["name"]
            if not path.exists():
                url = f"https://archive.ubuntu.com/ubuntu/{source['directory']}/{file['name']}"
                with urllib.request.urlopen(url, timeout=120) as response, path.open("wb") as destination:
                    shutil.copyfileobj(response, destination)
            if hashlib.sha512(path.read_bytes()).hexdigest() != file["sha512"]:
                raise ValueError(f"source hash mismatch: {path}")
        dsc = next(source_dir.glob("*.dsc"))
        build = source_dir / "build"
        if not build.exists():
            inside(["dpkg-source", "--no-check", "-x", f"/build/sources/{name}/{dsc.name}",
                    f"/build/sources/{name}/build"])
        # Hashes above pin the complete source; --no-check only skips the .dsc
        # signing-key lookup. No source content or build-dependency check is skipped.
        version = source["version"] + "~templaros26.04.1"
        changelog = build / "debian/changelog"
        if f"({version})" not in changelog.read_text().splitlines()[0]:
            changelog.write_text(
                f"{name} ({version}) resolute; urgency=medium\n\n"
                "  * Rebuild pinned LXQt source against TemplarOS Ubuntu Resolute libraries.\n\n"
                f" -- TemplarOS <build@templaros.org>  {formatdate(localtime=False)}\n\n"
                + changelog.read_text())
        inside(["apt-get", "build-dep", "-y", "--no-install-recommends", f"/build/sources/{name}/build"], env=environment)
        run(["chown", "-R", f"{uid}:{gid}", source_dir])
        stamp = source_dir / "built-version"
        if not stamp.exists() or stamp.read_text().strip() != version:
            inside(["setpriv", f"--reuid={uid}", f"--regid={gid}", "--clear-groups",
                    "/bin/sh", "-c", 'cd "$1"; exec dpkg-buildpackage -b -us -uc', "build",
                    f"/build/sources/{name}/build"], env=environment)
            stamp.write_text(version + "\n")
            os.chown(stamp, uid, gid)
        debs = sorted(source_dir.glob("*.deb"))
        if not debs:
            raise ValueError(f"no binary packages built for {name}")
        inside(["apt-get", "install", "-y", "--no-install-recommends",
                *[f"/build/sources/{name}/{deb.name}" for deb in debs]], env=environment)
        for deb in debs:
            destination = output / deb.name
            shutil.copyfile(deb, destination)
            os.chown(destination, uid, gid)
        print(f"BUILT {name} {version}", flush=True)
    write_result(args.work, lock, uid, gid)
    print(f"LXQt backport packages: {output}", flush=True)


if __name__ == "__main__":
    main()
