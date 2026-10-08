#!/usr/bin/env python3
"""Exact-content extraction regressions; no compiler or live desktop required."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("local_source", ROOT / "scripts/build-local-source-deb.py")
source = importlib.util.module_from_spec(spec)
spec.loader.exec_module(source)


class SourceClosureTests(unittest.TestCase):
    def bundle(self, root):
        stage = root / "stage"
        stage.mkdir()
        app = stage / "sources/LegeOS"
        app.mkdir(parents=True)
        (app / "Cargo.lock").write_text("version = 4\n")
        fonts = stage / "sources/pixelkit/crates/pixelkit-text/test-fonts"
        fonts.mkdir(parents=True)
        # Omission protection is content-based, including non-Cargo assets.
        (fonts / "font.ttf").write_bytes(b"retained asset bytes")
        (stage / "vendor").mkdir()
        (stage / "cargo-vendor-config.toml").write_text(
            '[source.crates-io]\nreplace-with = "vendored-sources"\n'
            '[source.vendored-sources]\ndirectory = "__VENDOR__"\n')
        provenance = {"package": "templaros-workspace", "application": "LegeOS",
                      "bundle_content_sha256": source.closure_hash(source.inventory(stage))}
        (stage / "source-provenance.json").write_text(json.dumps(provenance))
        return stage

    def test_stale_archive_pin_does_not_publish_destination(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stage = self.bundle(root)
            archive, digest = source.archive_bundle(stage, root, "workspace")
            destination = root / "prepared"
            with self.assertRaisesRegex(ValueError, "exact pin"):
                source.extract_source_archive(archive, destination, "0" * 64)
            self.assertFalse(destination.exists())
            source.extract_source_archive(archive, destination, digest)
            self.assertIn('../../vendor', (destination / "sources/LegeOS/.cargo/config.toml").read_text())

    def test_omitted_embedded_asset_rejects_bundle_even_with_matching_archive_hash(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stage = self.bundle(root)
            (stage / "sources/pixelkit/crates/pixelkit-text/test-fonts/font.ttf").unlink()
            archive, digest = source.archive_bundle(stage, root, "workspace")
            destination = root / "prepared"
            with self.assertRaisesRegex(ValueError, "content does not match"):
                source.extract_source_archive(archive, destination, digest)
            self.assertFalse(destination.exists())

    def test_existing_checkout_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stage = self.bundle(root)
            archive, digest = source.archive_bundle(stage, root, "workspace")
            destination = root / "checkout"
            destination.mkdir()
            (destination / "wip").write_text("owner work")
            with self.assertRaisesRegex(ValueError, "never overwritten"):
                source.extract_source_archive(archive, destination, digest)
            self.assertEqual((destination / "wip").read_text(), "owner work")

    def test_prepared_sibling_layout_and_vendor_config_remain_relocatable(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stage = self.bundle(root)
            archive, digest = source.archive_bundle(stage, root, "workspace")
            destination = root / "prepared"
            source.extract_source_archive(archive, destination, digest)
            relocated = root / "relocated"
            destination.rename(relocated)
            app = relocated / "sources/LegeOS"
            config = source.tomllib.loads((app / ".cargo/config.toml").read_text())
            vendor = (app / config["source"]["vendored-sources"]["directory"]).resolve()
            self.assertEqual(vendor, relocated / "vendor")
            self.assertEqual((app / "../pixelkit/crates/pixelkit-text/test-fonts/font.ttf").read_bytes(),
                             b"retained asset bytes")
            self.assertFalse((relocated / ".cargo-home").exists())

    def test_native_prerequisite_filter_excludes_only_the_checked_rust_tools(self):
        dependencies = source.native_build_dependencies(ROOT / "debian/control")
        self.assertFalse(any(source.re.fullmatch(r"(?:cargo|rustc)(?:\s*\([^)]*\))?", d)
                             for d in dependencies))
        for package in ("rustup", "python3", "libarchive-dev", "libclang-dev", "libudev-dev",
                        "libseat-dev", "libgbm-dev", "libpixman-1-dev", "libdisplay-info-dev"):
            self.assertIn(package, dependencies)
        self.assertIn("libinput-dev (>= 1.23)", dependencies)

    def test_nested_debhelper_cargo_uses_its_own_closed_vendor_graph(self):
        # Resolve an actual offline registry dependency without compiling. A
        # Cargo-home config alone loses to the enclosing prepared workspace;
        # a nearest rebuild config must also protect ordinary debhelper calls.
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            parent_config = root / ".cargo"
            parent_config.mkdir()
            unrelated_vendor = root / "outer-vendor"
            unrelated_vendor.mkdir()
            parent_config.joinpath("config.toml").write_text(
                '[source.crates-io]\nreplace-with = "vendored-sources"\n'
                f'[source.vendored-sources]\ndirectory = "{unrelated_vendor}"\n')
            rebuild = root / "target/source-builds/rebuild"
            app = rebuild / "sources/viewer"
            app.joinpath("src").mkdir(parents=True)
            app.joinpath("src/lib.rs").write_text("")
            app.joinpath("Cargo.toml").write_text(
                '[package]\nname = "closed-viewer-fixture"\nversion = "0.1.0"\n'
                'edition = "2021"\n[dependencies]\nclosed-only-fixture = "=9.9.9"\n')
            vendor = rebuild / "vendor/closed-only-fixture"
            vendor.joinpath("src").mkdir(parents=True)
            vendor.joinpath("src/lib.rs").write_text("")
            vendor.joinpath("Cargo.toml").write_text(
                '[package]\nname = "closed-only-fixture"\nversion = "9.9.9"\nedition = "2021"\n')
            vendor.joinpath(".cargo-checksum.json").write_text('{"files": {}, "package": null}')
            rebuild.joinpath("cargo-vendor-config.toml").write_text(
                '[source.crates-io]\nreplace-with = "vendored-sources"\n'
                '[source.vendored-sources]\ndirectory = "__VENDOR__"\n')
            home = source.configure_vendor(rebuild)
            environment = dict(os.environ, CARGO_HOME=str(home), CARGO_NET_OFFLINE="true")
            args = ["cargo", "generate-lockfile", "--offline"]
            failed = subprocess.run(args, cwd=app, env=environment, text=True, capture_output=True)
            self.assertNotEqual(failed.returncode, 0, failed.stdout + failed.stderr)
            self.assertIn(str(unrelated_vendor), failed.stderr)
            source.shutil.rmtree(home)
            source.configure_vendor(rebuild, build_config=True)
            subprocess.run(args, cwd=app, env=environment, check=True, text=True, capture_output=True)
            resolved = json.loads(subprocess.check_output(
                ["cargo", "metadata", "--locked", "--offline", "--format-version", "1"],
                cwd=app, env=environment, text=True))
            dependency = next(p for p in resolved["packages"] if p["name"] == "closed-only-fixture")
            self.assertEqual(Path(dependency["manifest_path"]), vendor / "Cargo.toml")


if __name__ == "__main__":
    unittest.main()
