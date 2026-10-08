#!/usr/bin/python3
"""Unprivileged contract/owned-temp-tree tests; not Btrfs, PAM or boot proof."""
import base64
import contextlib
import copy
import fcntl
import importlib.util
import os
from pathlib import Path
import tempfile
import shutil
import subprocess
import tomllib
import unittest

REPOSITORY = Path(__file__).resolve().parents[2]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, REPOSITORY / path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


ACCOUNTS = module("templaros_accounts", "installer/account-maintenance.py")
LAYOUT = module("templaros_layout", "installer/deployment-layout.py")
LOGIN_GATE = module("templaros_login_gate", "installer/account-login-gate.py")
DECLARATION = tomllib.loads((REPOSITORY / "distro/deploy/layout.toml").read_text())


@contextlib.contextmanager
def secret_stdin(payload, sealed=True):
    original = os.dup(0)
    descriptor = os.memfd_create("templaros-contract-test", os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING)
    try:
        os.write(descriptor, payload)
        if sealed:
            fcntl.fcntl(descriptor, fcntl.F_ADD_SEALS,
                fcntl.F_SEAL_WRITE | fcntl.F_SEAL_GROW | fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_SEAL)
        os.dup2(descriptor, 0)
        yield
    finally:
        os.dup2(original, 0)
        os.close(original)
        os.close(descriptor)


class SecretAndRecoveryContract(unittest.TestCase):
    def test_actual_same_filesystem_bind_with_escaped_mountpoint(self):
        if not shutil.which("bwrap"):
            self.skipTest("bwrap unavailable; native mapped-root mount proof requires it")
        with tempfile.TemporaryDirectory(prefix="templaros-bind-contract-") as directory:
            source = Path(directory) / "source"
            target = Path(directory) / "target with space\\backslash"
            unmounted = Path(directory) / "not-mounted"
            for path in (source, target, unmounted):
                path.mkdir()
            code = """
import importlib.util, os, sys
from pathlib import Path
for script in sys.argv[3:]:
    spec = importlib.util.spec_from_file_location('owned_gate', script)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    assert not os.path.ismount(sys.argv[1]), 'not a same-filesystem bind'
    assert module.is_mountpoint(Path(sys.argv[1]))
    assert not module.is_mountpoint(Path(sys.argv[2]))
"""
            result = subprocess.run(["bwrap", "--unshare-user", "--uid", "0", "--gid", "0",
                "--die-with-parent", "--ro-bind", "/", "/", "--proc", "/proc",
                "--bind", str(source), str(target), "/usr/bin/python3", "-I", "-c", code,
                str(target), str(unmounted), str(REPOSITORY / "installer/account-maintenance.py"),
                str(REPOSITORY / "installer/account-login-gate.py")], capture_output=True, text=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_colon_is_valid_and_transport_is_immutable_bounded_utf8(self):
        with secret_stdin(b"test:colon"):
            value = ACCOUNTS.read_secret()
            self.assertEqual(value, b"test:colon")
            value[:] = b"\0" * len(value)
        for payload in (b"", b"x" * 1025, b"bad\0value", b"bad\nvalue", b"bad\rvalue", b"\xff"):
            with self.subTest(payload_length=len(payload)), secret_stdin(payload):
                with self.assertRaises(RuntimeError):
                    ACCOUNTS.read_secret()
        with secret_stdin(b"unsealed", sealed=False):
            with self.assertRaises(RuntimeError):
                ACCOUNTS.read_secret()

    def intent(self):
        shadow = base64.b64encode(b"caller:$6$test:20000:0:99999:7:::\n").decode()
        return {"schema": 1, "phase": "prepared", "scratch": "@deployments/.accounts-" + "a" * 32,
            "roots": [{"subvolume": "@", "read_only": True, "mode": 0o640, "uid": 0, "gid": 42,
                "old_shadow": shadow, "new_shadow": shadow, "attributes": {}}]}

    def test_malformed_future_or_work_recovery_has_no_mutation_authority(self):
        template = "@deployments/{id}"
        ACCOUNTS.validate_intent(self.intent(), template)
        for key, value in (("schema", 2), ("schema", True), ("scratch", "@"), ("scratch", "@home")):
            intent = self.intent()
            intent[key] = value
            with self.subTest(key=key, value=value), self.assertRaises(RuntimeError):
                ACCOUNTS.validate_intent(intent, template)
        for key, value in (("subvolume", "@home"), ("subvolume", "@deployments/../home"),
                           ("read_only", "true"), ("mode", 0o666), ("uid", 1000)):
            intent = self.intent()
            intent["roots"][0][key] = value
            with self.subTest(key=key), self.assertRaises(RuntimeError):
                ACCOUNTS.validate_intent(intent, template)
        preparing = self.intent()
        preparing.update(phase="preparing", roots=[])
        ACCOUNTS.validate_intent(preparing, template)


class LayoutContract(unittest.TestCase):
    def test_package_database_and_work_cannot_be_state_binds(self):
        LAYOUT.bindings(DECLARATION, True)
        for target in ("/var", "/etc", "/home", "/var/lib", "/var/lib/dpkg/status", "/var/cache/apt/archives"):
            declaration = copy.deepcopy(DECLARATION)
            declaration["state"]["path"].append({"source": target, "target": target, "tier": "state", "owner": "hostd"})
            with self.subTest(target=target), self.assertRaises(RuntimeError):
                LAYOUT.bindings(declaration, True)

    def test_offline_preparation_preserves_only_gpu_mode_and_root_pairing(self):
        with tempfile.TemporaryDirectory(prefix="templaros-layout-contract-") as directory:
            root = Path(directory)
            hardware = root / "etc/templaros/hardware"
            hardware.mkdir(parents=True)
            (hardware / "gpu-mode").write_text("hybrid\n")
            (hardware / "version-lock.toml").write_text('nvidia_lrm = "paired-test"\n')
            (root / "etc/fstab").write_text("UUID=test / btrfs rw,subvol=@ 0 1\n")
            LAYOUT.prepare(root, DECLARATION, True)
            store = root / DECLARATION["state"]["store_mount"].lstrip("/")
            self.assertEqual((store / "var/lib/templaros/hardware/gpu-mode").read_text(), "hybrid\n")
            self.assertFalse((store / "var/lib/templaros/hardware/version-lock.toml").exists())
            self.assertEqual((hardware / "version-lock.toml").read_text(), 'nvidia_lrm = "paired-test"\n')
            fstab = (root / "etc/fstab").read_text()
            self.assertIn("subvol=@,ro", fstab)
            self.assertIn("/var/lib/templaros/state", fstab)
            self.assertNotIn("\t/var\t", fstab)
            (hardware / "gpu-mode").write_text("changed-legacy\n")
            LAYOUT.prepare(root, DECLARATION, True)
            self.assertEqual((store / "var/lib/templaros/hardware/gpu-mode").read_text(), "hybrid\n")

    def test_legacy_whole_var_refuses_before_creating_state(self):
        with tempfile.TemporaryDirectory(prefix="templaros-layout-legacy-") as directory:
            root = Path(directory)
            (root / "etc").mkdir()
            (root / "etc/fstab").write_text("UUID=test /var btrfs rw,subvol=@var 0 0\n")
            with self.assertRaises(RuntimeError):
                LAYOUT.prepare(root, DECLARATION, False)
            self.assertFalse((root / "var/lib/templaros/state").exists())


if __name__ == "__main__":
    unittest.main()
