#!/usr/bin/env python3
"""Close locked local Rust sources + vendor inputs; build or extract that exact snapshot."""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import shutil
import re
import subprocess
import sys
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
EXCLUDED = {".git", "target", ".akr", ".agent", ".codegraph", "node_modules", ".DS_Store",
            ".claude", ".codex", ".opencode", ".cache", ".cargo-home", "artifacts",
            "credentials", "credentials.toml", "runtime.json"}
WORKSPACE_FILES = {"Cargo.toml", "Cargo.lock", "rust-toolchain", "rust-toolchain.toml"}


def ignore_private_inputs(directory, names):
    return [name for name in names if name in EXCLUDED or name.startswith(("._", ".env"))]


def run(args, cwd, **kwargs):
    return subprocess.run(args, cwd=cwd, check=True, text=True, **kwargs)


def metadata(source, cargo_home=None, target=None):
    args = ["cargo", "metadata", "--format-version", "1", "--locked", "--offline"]
    if target:
        args += ["--filter-platform", target]
    if cargo_home and (cargo_home / "config.toml").is_file():
        args += ["--config", str(cargo_home / "config.toml")]
    environment = dict(os.environ)
    if cargo_home:
        environment["CARGO_HOME"] = str(cargo_home)
    return json.loads(run(args, source, capture_output=True, env=environment).stdout)

def workspace_owner(manifest):
    """Keep workspace inheritance, members, build scripts and embedded assets intact."""
    directory = manifest.parent
    for parent in [directory, *directory.parents]:
        candidate = parent / "Cargo.toml"
        if candidate.is_file():
            document = tomllib.loads(candidate.read_text())
            if "workspace" in document:
                return parent
    return directory


def inventory(directory):
    entries = []
    for path in sorted(directory.rglob("*")):
        relative = path.relative_to(directory).as_posix()
        if path.is_symlink():
            resolved = path.resolve(strict=True)
            if not resolved.is_relative_to(directory.resolve()):
                raise ValueError(f"source symlink escapes closure: {relative}")
            entries.append([relative, "symlink", os.readlink(path)])
        elif path.is_file():
            # Reproduction depends on executability, not the host's writable umask.
            mode = 0o755 if path.stat().st_mode & 0o100 else 0o644
            entries.append([relative, "file", mode,
                            hashlib.sha256(path.read_bytes()).hexdigest()])
    return entries


def closure_hash(entries):
    return hashlib.sha256(json.dumps(entries, separators=(",", ":")).encode()).hexdigest()


def copy_sources(source, stage, companions=(), complete_application=False):
    graphs = [metadata(app) for app in (source, *companions)]
    resolved = graphs[0]
    all_packages = {p["id"]: p for graph in graphs for p in graph["packages"]}
    packages = [p for p in all_packages.values() if p["source"] is None]
    owners = {workspace_owner(Path(p["manifest_path"]).resolve()) for p in packages}
    owners = {p for p in owners if not any(p != other and p.is_relative_to(other) for other in owners)}
    base = Path(os.path.commonpath([str(p) for p in owners]))
    if base == Path(base.anchor):
        raise ValueError("local source roots have no bounded common parent")
    source_relative = source.relative_to(base)
    if source_relative.as_posix() != source.name:
        raise ValueError("application must be a direct sibling of the source closure roots")
    records = []
    for owner in sorted(owners):
        relative = owner.relative_to(base)
        destination = stage / "sources" / relative
        destination.mkdir(parents=True, exist_ok=True)
        if complete_application and owner == source:
            shutil.copytree(source, destination, dirs_exist_ok=True, symlinks=True,
                            ignore=ignore_private_inputs)
        # Keep selected dependency trees, not unrelated sibling workspace tools
        # or corpora; the first-party application was copied whole above.
        package_directories = {Path(p["manifest_path"]).resolve().parent for p in packages
                               if Path(p["manifest_path"]).resolve().is_relative_to(owner)}
        package_directories = {p for p in package_directories
                               if not any(p != other and p.is_relative_to(other) for other in package_directories)}
        for directory in (() if complete_application and owner == source else sorted(package_directories)):
            package_dest = stage / "sources" / directory.relative_to(base)
            shutil.copytree(directory, package_dest, dirs_exist_ok=True, symlinks=True,
                            ignore=ignore_private_inputs)
        for path in owner.iterdir():
            if path.is_file() and (path.name in WORKSPACE_FILES or
                                   path.name.startswith(("LICENSE", "COPYING", "NOTICE"))):
                shutil.copy2(path, destination / path.name)
        owning_manifest = tomllib.loads((owner / "Cargo.toml").read_text())
        owning_license = owning_manifest.get("workspace", {}).get("package", {}).get("license-file")
        if owning_license:
            path = (owner / owning_license).resolve(strict=True)
            if not path.is_relative_to(owner):
                raise ValueError(f"workspace license escapes owning source: {owner}")
            target = destination / path.relative_to(owner)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
        # A workspace license-file can live below its root (still265 uses
        # x265_4.1/COPYING), outside every selected package directory.
        for package in packages:
            license_file = package.get("license_file")
            if license_file:
                path = Path(license_file).resolve(strict=True)
                if path.is_relative_to(owner):
                    target = destination / path.relative_to(owner)
                    target.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(path, target)
        # No Git assumption: content identifies even a completely unversioned input.
        content = closure_hash(inventory(destination))
        record = {"path": relative.as_posix(), "content_sha256": content}
        git = subprocess.run(["git", "-C", str(owner), "rev-parse", "HEAD"],
                             text=True, capture_output=True)
        if git.returncode == 0:
            record["git_head"] = git.stdout.strip()
            dirty = subprocess.run(["git", "-C", str(owner), "status", "--porcelain", "--", "."],
                                   text=True, capture_output=True, check=True)
            record["git_dirty"] = bool(dirty.stdout)
        else:
            record["git_head"] = None
        records.append(record)
    app = stage / "sources" / source_relative
    # Resolving again from the copy proves that no path dependency still points
    # at the original sibling checkouts; metadata alone does not prove assets.
    copied_apps = [app, *(stage / "sources" / companion.relative_to(base)
                         for companion in companions)]
    for copied_app in copied_apps:
        copied = metadata(copied_app)
        for package in copied["packages"]:
            if package["source"] is None and not Path(package["manifest_path"]).resolve().is_relative_to(stage):
                raise ValueError(f"path dependency escapes copied closure: {package['name']}")
    vendor_args = ["cargo", "vendor", "--locked", "--offline"]
    for copied_app in copied_apps[1:]:
        vendor_args += ["--sync", str(copied_app / "Cargo.toml")]
    vendor = run([*vendor_args, str(stage / "vendor")], app, capture_output=True)
    config = vendor.stdout.replace(str(stage / "vendor"), "__VENDOR__")
    (stage / "cargo-vendor-config.toml").write_text(config)
    resolved = dict(resolved, packages=list(all_packages.values()))
    return source_relative.as_posix(), records, resolved


def archive_bundle(stage, output, name):
    # Stable header metadata and gzip timestamp; content, not a fictional commit,
    # is the archive identity. Archive includes every file consumed by the build.
    with tempfile.NamedTemporaryFile(dir=output, suffix=".tar.gz", delete=False) as temporary:
        temporary_path = Path(temporary.name)
        with gzip.GzipFile(filename="", mode="wb", fileobj=temporary, mtime=0) as compressed:
            with tarfile.open(fileobj=compressed, mode="w") as archive:
                for path in sorted(stage.rglob("*")):
                    info = archive.gettarinfo(str(path), path.relative_to(stage).as_posix())
                    info.uid = info.gid = 0
                    info.uname = info.gname = ""
                    info.mtime = 0
                    if info.isfile():
                        info.mode = 0o755 if info.mode & 0o100 else 0o644
                    elif info.isdir():
                        info.mode = 0o755
                    if info.isfile():
                        with path.open("rb") as data:
                            archive.addfile(info, data)
                    else:
                        archive.addfile(info)
    digest = hashlib.sha256(temporary_path.read_bytes()).hexdigest()
    destination = output / f"{name}-{digest}.tar.gz"
    temporary_path.replace(destination)
    destination.with_suffix(destination.suffix + ".sha256").write_text(f"{digest}  {destination.name}\n")
    return destination, digest


def unpack_bundle(source, stage):
    # Archives are build inputs, not trusted extraction instructions.
    with tarfile.open(source, "r:gz") as archive:
        for member in archive.getmembers():
            destination = stage / member.name
            if not destination.resolve().is_relative_to(stage) or member.isdev() or member.isfifo():
                raise ValueError(f"unsafe source archive member: {member.name}")
            if member.issym() or member.islnk():
                target = destination.parent / member.linkname if member.issym() else stage / member.linkname
                if not target.resolve().is_relative_to(stage):
                    raise ValueError(f"source archive link escapes closure: {member.name}")
        archive.extractall(stage, filter="data")
    provenance = json.loads((stage / "source-provenance.json").read_text())
    expected = provenance["bundle_content_sha256"]
    actual = inventory(stage)
    actual = [e for e in actual if e[0] != "source-provenance.json"]
    if closure_hash(actual) != expected:
        raise ValueError("source archive content does not match its provenance")
    return provenance


def configure_vendor(stage, *, build_config=False):
    home = stage / ".cargo-home"
    home.mkdir()
    config = (stage / "cargo-vendor-config.toml").read_text().replace("__VENDOR__", str(stage / "vendor"))
    (home / "config.toml").write_text(config)
    if build_config:
        # Cargo searches ancestor .cargo directories before CARGO_HOME. The
        # temporary rebuild can itself live beneath a prepared workspace whose
        # vendor graph differs from this application's lock. Keep the closed
        # graph authoritative for ordinary Cargo calls made by debhelper too,
        # which do not receive metadata()'s explicit --config argument.
        local = stage / ".cargo"
        local.mkdir(exist_ok=True)
        if (local / "config").exists() or (local / "config.toml").exists():
            raise ValueError("closed rebuild already has a Cargo root config")
        (local / "config.toml").write_text(config)
    return home

def archive_digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def extract_source_archive(source, destination, expected):
    if not expected or archive_digest(source) != expected:
        raise ValueError("source archive SHA256 does not match the required exact pin")
    if destination.exists():
        raise ValueError("extraction destination must not exist; existing checkouts are never overwritten")
    destination.parent.mkdir(parents=True, exist_ok=True)
    # Publish only a completely validated extraction, never a partial checkout.
    with tempfile.TemporaryDirectory(dir=destination.parent, prefix="templaros-extract-") as work:
        stage = Path(work) / "bundle"
        stage.mkdir()
        provenance = unpack_bundle(source, stage)
        if provenance.get("package") != "templaros-workspace":
            raise ValueError("developer preparation requires a complete TemplarOS workspace archive")
        configure_vendor(stage)
        app = stage / "sources" / provenance["application"]
        config = (stage / ".cargo-home/config.toml").read_text()
        # Relative to the application, so the prepared tree can be relocated.
        config = config.replace(str(stage / "vendor"), "../../vendor")
        (app / ".cargo").mkdir(exist_ok=True)
        with (app / ".cargo/config.toml").open("a") as output:
            output.write("\n" + config)
        (stage / ".cargo-home/config.toml").unlink()
        (stage / ".cargo-home").rmdir()
        stage.rename(destination)
    return provenance


def native_build_dependencies(control):
    text = control.read_text()
    if not re.search(r"^Rules-Requires-Root: no$", text, re.MULTILINE):
        raise ValueError("local source packaging must explicitly support rootless builds")
    match = re.search(r"^Build-Depends: (.*(?:\n[ \t]+.*)*)", text, re.MULTILINE)
    if not match:
        raise ValueError("local overlay must declare Build-Depends")
    dependencies = [dependency.strip() for dependency in match[1].replace("\n", " ").split(",")]
    # Only these exact packages can be fulfilled by independently checked Rust binaries.
    return [d for d in dependencies if not re.fullmatch(r"(?:cargo|rustc)(?:\s*\([^)]*\))?", d)]


def workspace_archive(args):
    if args.native_build_depends or args.check_build_environment:
        if args.extract or args.expect_sha256 or not args.source or not args.source.is_dir():
            raise ValueError("workspace prerequisite checks require --source PREPARED_APPLICATION")
        app = args.source.resolve(strict=True)
        if args.native_build_depends:
            print(", ".join(native_build_dependencies(app / "debian/control")))
        else:
            home = app / "debian/cargo_home"
            home.mkdir(parents=True, exist_ok=True)
            preview = app / "helpers/templar-preview-codecs"
            print(json.dumps(checked_build_environment(app, app, home, (preview,)), indent=2))
        return
    if args.extract:
        if not args.source or not args.source.is_file():
            raise ValueError("--extract requires --source ARCHIVE and --expect-sha256 PIN")
        provenance = extract_source_archive(args.source.resolve(), args.extract.resolve(),
                                            args.expect_sha256)
        print(f"prepared source: {args.extract}/sources/{provenance['application']}")
        return
    if args.expect_sha256:
        raise ValueError("--expect-sha256 is used only with --extract")
    source = (args.source or ROOT).resolve(strict=True)
    if not source.is_dir():
        raise ValueError("workspace closure requires a source directory, or --extract")
    output = ROOT / "target/source-archives"
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=output, prefix="templaros-workspace-") as work:
        stage = Path(work) / "bundle"
        stage.mkdir()
        preview = source / "helpers/templar-preview-codecs"
        app, owners, resolved = copy_sources(source, stage, (preview,), True)
        copied_app = stage / "sources" / app
        closed_manifests = ["Cargo.toml", "helpers/templar-preview-codecs/Cargo.toml"]
        retained_unvendored = []
        for manifest in sorted(copied_app.rglob("Cargo.toml")):
            relative = manifest.relative_to(copied_app).as_posix()
            if relative in closed_manifests or "workspace" not in tomllib.loads(manifest.read_text()):
                continue
            lock = manifest.with_name("Cargo.lock")
            retained_unvendored.append({
                "manifest": relative,
                "cargo_lock_sha256": archive_digest(lock) if lock.is_file() else None,
            })
        provenance = {
            "schema_version": 1, "package": "templaros-workspace", "application": app,
            "release_origin": None, "release_pin": None, "source_roots": owners,
            "dependencies": [{k: p[k] for k in ("name", "version", "source", "license", "rust_version")}
                             for p in resolved["packages"]],
            "cargo_lock_sha256": hashlib.sha256((copied_app / "Cargo.lock").read_bytes()).hexdigest(),
            "companion_locks": {"helpers/templar-preview-codecs/Cargo.lock":
                                hashlib.sha256((copied_app / "helpers/templar-preview-codecs/Cargo.lock").read_bytes()).hexdigest()},
            "closed_manifests": closed_manifests,
            "retained_unvendored_workspaces": retained_unvendored,
            "bundle_content_sha256": closure_hash(inventory(stage)),
        }
        (stage / "source-provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
        home = configure_vendor(stage)
        for application in (copied_app, copied_app / "helpers/templar-preview-codecs"):
            for package in metadata(application, home)["packages"]:
                if not Path(package["manifest_path"]).resolve().is_relative_to(stage):
                    raise ValueError(f"dependency remains outside closed archive: {package['name']}")
        shutil.rmtree(home)
        archive, digest = archive_bundle(stage, output, "templaros-workspace")
        print(f"closed source archive: {archive.relative_to(ROOT)}")
        print(f"archive SHA256: {digest}")
        print(f"bundle content SHA256: {provenance['bundle_content_sha256']}")
        for workspace in retained_unvendored:
            print(f"retained source only (graph not vendored): {workspace['manifest']}")
        print("No public origin/pin or permission to publish private source is asserted.")


def checked_build_environment(stage, app, cargo_home, companions=()):
    """Rustup is a real developer toolchain, not a dpkg-installed Rust package."""
    verbose = run(["rustc", "-vV"], stage, capture_output=True).stdout
    host = next(line.removeprefix("host: ") for line in verbose.splitlines() if line.startswith("host: "))
    architecture = run(["dpkg", "--print-architecture"], stage, capture_output=True).stdout.strip()
    expected_arch = run(["dpkg-architecture", f"-t{host.replace('-unknown', '')}", "-qDEB_HOST_ARCH"],
                        stage, capture_output=True).stdout.strip()
    if architecture != expected_arch or "-linux-" not in host:
        raise ValueError(f"compiler host {host} does not match native Debian architecture {architecture}")
    if os.environ.get("CARGO_BUILD_TARGET", host) != host:
        raise ValueError("local source package requires the native Cargo build target")
    reachable_packages = {}
    closure_root = app.parent.parent if companions else stage
    for application in (app, *companions):
        resolved = metadata(application, cargo_home, host)
        for package in resolved["packages"]:
            if not Path(package["manifest_path"]).resolve().is_relative_to(closure_root):
                raise ValueError(f"dependency escapes prepared closure: {package['name']}")
        nodes = {node["id"]: node for node in resolved["resolve"]["nodes"]}
        reachable = set()
        if companions:
            pending = list(resolved["workspace_members"])
        else:
            # Registered external applications still check their native package
            # graph, not unrelated executables in their owning workspace.
            pending = [next(p["id"] for p in resolved["packages"]
                            if p["manifest_path"] == str(application / "Cargo.toml"))]
        while pending:
            package = pending.pop()
            if package in reachable:
                continue
            reachable.add(package)
            for dependency in nodes[package]["deps"]:
                if any(kind["kind"] != "dev" for kind in dependency["dep_kinds"]):
                    pending.append(dependency["pkg"])
        reachable_packages.update((p["id"], p) for p in resolved["packages"] if p["id"] in reachable)
    minimum = max(tuple(int(n) for n in (p.get("rust_version") or "0").split("."))
                  for p in reachable_packages.values())
    versions = {"compiler_host": host, "dpkg_architecture": architecture,
                "native_graph_msrv": ".".join(map(str, minimum)),
                "dependency_substitution": "rustc/cargo Build-Depends fulfilled by checked native developer binaries"}
    for command in ("rustc", "cargo"):
        version = run([command, "--version"], stage, capture_output=True).stdout.strip()
        match = re.match(r"\w+ (\d+)\.(\d+)\.(\d+)", version)
        if not match or tuple(map(int, match.groups())) < minimum:
            raise ValueError(f"{command} must satisfy locked graph Rust {'.'.join(map(str, minimum))}: {version}")
        versions[command] = version
    native = native_build_dependencies(stage / "debian/control")
    run(["dpkg-checkbuilddeps", "-d", ", ".join(native)], stage)
    return versions


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("name")
    parser.add_argument("--source", type=Path, help="local source directory or previously closed .tar.gz archive")
    parser.add_argument("--deb-version", help="the normal build-set version authority")
    parser.add_argument("--dry-run", action="store_true", help="close and inspect source archive, without compiling")
    parser.add_argument("--extract", type=Path, help="workspace only: prepare a new isolated directory from an archive")
    parser.add_argument("--expect-sha256", help="workspace only: required trusted archive pin for extraction")
    parser.add_argument("--native-build-depends", action="store_true",
                        help="workspace only: print Debian prerequisites excluding checked Rust tools")
    parser.add_argument("--check-build-environment", action="store_true",
                        help="workspace only: check native compiler and Debian prerequisites")
    args = parser.parse_args()
    if args.name == "workspace":
        workspace_archive(args)
        return
    if args.extract or args.expect_sha256 or args.native_build_depends or args.check_build_environment:
        raise ValueError("workspace preparation/prerequisite options require workspace")
    manifest = tomllib.loads((ROOT / "distro/forks/manifest.toml").read_text())
    registration = manifest[args.name]
    if registration.get("source_kind") != "local":
        raise ValueError("only explicitly registered local sources use this builder")
    source = (args.source or ROOT / registration["source"]).resolve(strict=True)
    overlay = ROOT / "distro/forks" / args.name / "debian"
    output = ROOT / "target/source-archives"
    output.mkdir(parents=True, exist_ok=True)
    # Full source + vendor copies and release objects can exceed a tmpfs quota.
    # Keep default build scratch on the native target filesystem, not /tmp.
    work_root = Path(os.environ.get("TMPDIR", ROOT / "target/native/ubuntu-26.04/source-builds")).resolve()
    work_root.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=work_root, prefix=f"templaros-local-{args.name}-") as work:
        stage = Path(work) / "bundle"
        stage.mkdir()
        if source.is_file():
            provenance = unpack_bundle(source, stage)
            if provenance["package"] != registration["package"]:
                raise ValueError("source archive is registered for a different package")
            digest = hashlib.sha256(source.read_bytes()).hexdigest()
            archive = output / f"{args.name}-{digest}.tar.gz"
            if source != archive:
                shutil.copyfile(source, archive)
            archive.with_suffix(archive.suffix + ".sha256").write_text(f"{digest}  {archive.name}\n")
        else:
            app, owners, resolved = copy_sources(source, stage)
            shutil.copytree(overlay, stage / "debian")
            provenance = {
                "schema_version": 1, "package": registration["package"], "application": app,
                "release_origin": None, "release_pin": None, "source_roots": owners,
                "dependencies": [{k: p[k] for k in ("name", "version", "source", "license", "rust_version")} for p in resolved["packages"]],
                "cargo_lock_sha256": hashlib.sha256((stage / "sources" / app / "Cargo.lock").read_bytes()).hexdigest(),
                "bundle_content_sha256": closure_hash(inventory(stage)),
            }
            (stage / "source-provenance.json").write_text(json.dumps(provenance, indent=2) + "\n")
            archive, digest = archive_bundle(stage, output, args.name)
        print(f"closed source archive: {archive.relative_to(ROOT)}", flush=True)
        print(f"archive SHA256: {digest}", flush=True)
        print("release checkout origin/pin: not supplied; archive content is the local reproduction authority", flush=True)
        # Use an empty Cargo home and the archived vendor tree: no host registry,
        # Git cache or uncopied sibling can accidentally satisfy a missing input.
        home = configure_vendor(stage)
        app = stage / "sources" / provenance["application"]
        isolated = metadata(app, home)
        for package in isolated["packages"]:
            if not Path(package["manifest_path"]).resolve().is_relative_to(stage):
                raise ValueError(f"dependency remains outside closed archive: {package['name']}")
        print(f"closed dependency inventory: {len(isolated['packages'])} packages, {len(provenance['source_roots'])} owning source roots", flush=True)
        if args.dry_run:
            print("dry run: complete source archive inspected; native compilation/package not executed")
            return
        # Build from the archive, not from the live snapshot that created it.
        rebuilt = Path(work) / "rebuild"
        rebuilt.mkdir()
        unpack_bundle(archive, rebuilt)
        changelog = rebuilt / "debian/changelog"
        head, rest = changelog.read_text().split("\n", 1)
        original_version = head[head.index("(") + 1:head.index(")")]
        version = args.deb_version or f"{original_version}~s{digest[:16]}"
        run(["dpkg", "--validate-version", version], rebuilt)
        head = head[:head.index("(") + 1] + version + head[head.index(")"):]
        changelog.write_text(head + "\n" + rest)
        build_home = configure_vendor(rebuilt, build_config=True)
        toolchain = checked_build_environment(rebuilt, rebuilt / "sources" / provenance["application"], build_home)
        # The developer toolchain is Rustup-managed, not installed through dpkg.
        # dh performs the same real binary-package steps after the independent
        # prerequisite checks above; no dpkg-buildpackage -d gate suppression.
        environment = dict(os.environ)
        # The checked native target is equivalent to Cargo's default host.
        # Drop its override so retained overlays still use target/release.
        environment.pop("CARGO_BUILD_TARGET", None)
        environment.update({"CARGO_HOME": str(rebuilt / ".cargo-home"),
                            "CARGO_TARGET_DIR": str(rebuilt / "target"),
                            "CARGO_NET_OFFLINE": "true", "DEB_RULES_REQUIRES_ROOT": "no"})
        run(["debian/rules", "binary"], rebuilt, env=environment)
        debs = list(Path(work).glob(f"{registration['package']}_*.deb"))
        if not debs:
            raise ValueError("rootless debhelper produced no registered package artifact")
        deb_dir = ROOT / "target/debian"
        deb_dir.mkdir(parents=True, exist_ok=True)
        for deb in debs:
            run(["dpkg-deb", "--info", str(deb)], ROOT)
            shutil.copyfile(deb, deb_dir / deb.name)
            (deb_dir / (deb.name + ".source.json")).write_text(json.dumps({
                "package": registration["package"], "version": version,
                "source_archive": archive.relative_to(ROOT).as_posix(), "source_archive_sha256": digest,
                "toolchain": toolchain,
            }, indent=2) + "\n")
            print(f"built: target/debian/{deb.name}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as error:
        if isinstance(error, subprocess.CalledProcessError) and error.stderr:
            print(error.stderr, file=sys.stderr)
        print(f"local source package failed: {error}", file=sys.stderr)
        sys.exit(1)
