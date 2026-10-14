#!/usr/bin/env python3
"""Exercise the installed X11 launcher's argv/environment without opening a VT."""
from pathlib import Path
import subprocess
import tempfile
import unittest

LAUNCHER = Path(__file__).resolve().parents[2] / "session/units/greetd/templar-x11-session"


class X11SessionTests(unittest.TestCase):
    def launch(self, vt, command="startlxqt", arguments=()):
        with tempfile.TemporaryDirectory(prefix="templar-x11-test-") as directory:
            fixture = Path(directory)
            for name in ("startlxqt", "desktop with spaces"):
                file = fixture / name
                file.write_text("#!/bin/sh\nexit 0\n")
                file.chmod(0o755)
            startx = fixture / "startx"
            startx.write_text(
                "#!/bin/sh\n"
                "printf '%s\\n' \"$@\" > /tmp/fixture/argv\n"
                "printf '%s\\n' \"$XDG_SESSION_TYPE\" \"${WAYLAND_DISPLAY-unset}\" "
                "\"${QT_QPA_PLATFORM-unset}\" \"${DISPLAY-unset}\" > /tmp/fixture/env\n")
            startx.chmod(0o755)
            result = subprocess.run([
                "bwrap", "--unshare-pid", "--unshare-net", "--ro-bind", "/", "/",
                "--tmpfs", "/tmp", "--tmpfs", "/usr/bin", "--ro-bind", "/usr/bin/dash", "/usr/bin/sh",
                "--ro-bind", str(startx), "/usr/bin/startx", "--bind", directory, "/tmp/fixture",
                "--setenv", "PATH", "/tmp/fixture", "--setenv", "XDG_VTNR", vt,
                "--setenv", "WAYLAND_DISPLAY", "greeter-wayland",
                "--setenv", "QT_QPA_PLATFORM", "wayland", "--setenv", "DISPLAY", ":0",
                str(LAUNCHER), command, *arguments,
            ], capture_output=True, text=True)
            argv = (fixture / "argv").read_text().splitlines() if (fixture / "argv").exists() else []
            env = (fixture / "env").read_text().splitlines() if (fixture / "env").exists() else []
            return result, argv, env

    def test_xorg_owns_the_login_vt_and_receives_literal_desktop_arguments(self):
        result, argv, env = self.launch("7", "/tmp/fixture/desktop with spaces", ("", "; touch /tmp/fixture/oops"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(argv, ["/tmp/fixture/desktop with spaces", "", "; touch /tmp/fixture/oops",
                                "--", "vt7", "-keeptty", "-nolisten", "tcp"])
        self.assertEqual(env, ["x11", "unset", "unset", "unset"])

    def test_path_lookup_resolves_startlxqt(self):
        result, argv, _ = self.launch("1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(argv[0], "/tmp/fixture/startlxqt")

    def test_missing_desktop_or_invalid_vt_never_launches_xorg(self):
        for vt, command in (("1", "missing-desktop"), ("0", "startlxqt"), ("7;true", "startlxqt")):
            with self.subTest(vt=vt, command=command):
                result, argv, _ = self.launch(vt, command)
                self.assertNotEqual(result.returncode, 0)
                self.assertTrue("unavailable" in result.stderr or "Invalid X11 login VT" in result.stderr, result.stderr)
                self.assertEqual(argv, [])


if __name__ == "__main__":
    unittest.main()
