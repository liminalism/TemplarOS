#!/usr/bin/env python3
"""Exercise the real maintainer script in an isolated filesystem namespace."""
from pathlib import Path
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[2]

class PamUpgrade(unittest.TestCase):
    def run_postinst(self, native, legacy, pending=None):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for name in ('etc/templaros', 'etc/alternatives', 'state/hostd/accounts', 'state/account-maintenance', 'bin'):
                (root / name).mkdir(parents=True)
            (root / 'etc/fstab').write_text('UUID=test /var btrfs subvol=@var 0 0\n' if legacy else '')
            if native:
                (root / 'etc/templaros/install.toml').touch()
            if pending:
                (root / 'state' / pending).write_text('{}')
            for name, body in [('install', 'exit 0'), ('pam-auth-update', 'printf "%s\\n" "$*"')]:
                p = root / 'bin' / name
                p.write_text('#!/bin/sh\n' + body + '\n')
                p.chmod(0o755)
            return subprocess.run(['bwrap', '--unshare-user', '--die-with-parent', '--ro-bind', '/', '/',
                '--tmpfs', '/var', '--dir', '/var/lib', '--dir', '/var/lib/templaros',
                '--ro-bind', str(root / 'etc'), '/etc',
                '--ro-bind', '/etc/alternatives', '/etc/alternatives', '--ro-bind', str(root / 'state'), '/var/lib/templaros',
                '--setenv', 'PATH', str(root / 'bin') + ':/usr/bin:/bin',
                '/bin/sh', str(REPO / 'packaging/templaros-hostd/postinst'), 'configure'],
                text=True, capture_output=True)

    def test_legacy_native_upgrade_removes_incompatible_profile(self):
        result = self.run_postinst(native=True, legacy=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('--remove templaros-account-recovery', result.stdout)
        self.assertNotIn('--enable', result.stdout)

    def test_pending_account_transaction_never_disables_gate(self):
        for pending in ('hostd/accounts/intent.json', 'account-maintenance/active.json'):
            with self.subTest(pending=pending):
                result = self.run_postinst(native=True, legacy=True, pending=pending)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('Pending TemplarOS account maintenance', result.stderr)
                self.assertNotIn('--remove', result.stdout)

    def test_new_layout_enables_gate(self):
        result = self.run_postinst(native=True, legacy=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('--enable templaros-account-recovery', result.stdout)

    def test_non_native_install_keeps_supported_gate(self):
        result = self.run_postinst(native=False, legacy=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('--enable templaros-account-recovery', result.stdout)

if __name__ == '__main__':
    unittest.main()
