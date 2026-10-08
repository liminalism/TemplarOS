#!/usr/bin/env python3
"""Owned, bus-free source-hook smoke. Never loads user rc/config or changes live settings.
Run: python3 tests/integration/garden-session-hooks.py --shell bash|zsh|fish
The templar test double is only a command recorder; real adapter projection/credential and
catalogue behavior are covered by Rust tests, not simulated here.
"""
import argparse
import errno
import fcntl
import os
from pathlib import Path
import pty
import select
import shlex
import shutil
import subprocess
import tempfile
import termios
import time

ROOT = Path(__file__).resolve().parents[2]
HOOKS = ROOT / "session/profile.d"


def interactive(command, script, environment, directory, prompt=None):
    # fish_postexec is emitted by the real terminal reader, not by `fish -i` on a pipe.
    master, slave = pty.openpty()
    process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave,
                               env=environment, cwd=directory, start_new_session=True,
                               preexec_fn=lambda: fcntl.ioctl(slave, termios.TIOCSCTTY, 0))
    os.close(slave)
    output = bytearray()
    deadline = time.monotonic() + 15

    def receive(marker=None):
        start = len(output)
        while marker is None or marker not in output[start:]:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([master], [], [], remaining)[0]:
                raise TimeoutError("owned interactive shell did not respond")
            try:
                data = os.read(master, 65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                data = b""
            if not data:
                if marker is not None:
                    raise AssertionError(f"shell exited before prompt: {output!r}")
                return
            output.extend(data)

    try:
        if prompt is None:
            os.write(master, script.encode())
        else:
            # Pace actual terminal input at prompt boundaries; callbacks can inspect
            # the producer journal before the next command or empty prompt cycle.
            receive(prompt)
            for text, inspect in script:
                if text is not None:
                    os.write(master, text.encode())
                    receive(prompt)
                if inspect is not None:
                    inspect()
            os.write(master, b"exit 0\n")
        receive()
        process.wait(timeout=max(.1, deadline - time.monotonic()))
    finally:
        os.close(master)
        if process.poll() is None:
            process.kill()
            process.wait()
    return subprocess.CompletedProcess(command, process.returncode, "", output.decode(errors="replace"))


def smoke(shell: str, histcontrol: str = "ignorespace") -> None:
    binary = shutil.which(shell)
    if binary is None:
        raise SystemExit(f"unavailable: {shell} is not installed")
    with tempfile.TemporaryDirectory(prefix="templaros-hook-owned-") as temporary:
        directory = Path(temporary)
        runtime = directory / "runtime"
        runtime.mkdir(mode=0o700)
        (runtime / "templaros-garden").mkdir(mode=0o700)
        recorder = directory / "templar"
        recorder.write_text("#!/bin/sh\ncase \"$1:$2\" in\n garden:journal-init) printf '%s/templaros-garden/shell-owned\\n' \"$XDG_RUNTIME_DIR\";;\n *) printf '%s\\n' \"$*\" >> \"$TRACE_FILE\";;\nesac\n")
        recorder.chmod(0o700)
        environment = dict(os.environ, HOME=str(directory), XDG_CONFIG_HOME=str(directory / "config"),
                           XDG_RUNTIME_DIR=str(runtime), XDG_CURRENT_DESKTOP="TemplarOS",
                           PATH=f"{directory}:/usr/bin:/bin", TRACE_FILE=str(directory / "trace"),
                           TEMPLAR_NOTIFY_MIN_SECONDS="999999", HISTFILE="/dev/null",
                           TERM="xterm", COLUMNS="100", LINES="30")
        if shell == "bash":
            rc = directory / "rc"
            rc.write_text(f"HISTCONTROL={shlex.quote(histcontrol)}\n__user_prompt() {{ printf 'user-prompt:%s\\n' \"$?\" >> \"$TRACE_FILE\"; }}\nPROMPT_COMMAND=(__user_prompt)\n" +
                          f"source {shlex.quote(str(HOOKS / '60-templaros-garden.sh'))}\n" +
                          f"source {shlex.quote(str(HOOKS / '61-templaros-completion.sh'))}\n")
            # --rcfile still loads Debian's system bash.bashrc, whose installed hooks can
            # satisfy our idempotence guards before the source-under-test is loaded.
            command = [binary, "--noprofile", "--norc", "-i"]
            script = f"source {shlex.quote(str(rc))}\n" + "set -o pipefail\nfalse | true\nexport TEMPLAR_GARDEN_FULL_COMMANDS=1\nprintf 'VISIBLE_SENTINEL\\n'\n printf 'PRIVATE_SENTINEL\\n'\nfalse\nexit 0\n"
        elif shell == "zsh":
            command = [binary, "-f", "-i"]
            script = (f"source {shlex.quote(str(HOOKS / '60-templaros-garden.zsh'))}\n" +
                      f"source {shlex.quote(str(HOOKS / '61-templaros-completion.zsh'))}\n" +
                      "setopt HIST_IGNORE_SPACE PIPE_FAIL\nfalse | true\nexport TEMPLAR_GARDEN_FULL_COMMANDS=1\nprintf 'VISIBLE_SENTINEL\\n'\n printf 'PRIVATE_SENTINEL\\n'\nfalse\nexit 0\n")
        else:
            command = [binary, "--no-config", "-i", "-C",
                       "set -g fish_function_path $__fish_data_dir/functions; function fish_prompt; end; " +
                       f"source {shlex.quote(str(HOOKS / '60-templaros-garden.fish'))}; " +
                       f"source {shlex.quote(str(HOOKS / '61-templaros-completion.fish'))}"]
            script = "false | true\nset -gx TEMPLAR_GARDEN_FULL_COMMANDS 1\nprintf 'VISIBLE_SENTINEL\\n'\n printf 'PRIVATE_SENTINEL\\n'\nfalse\nexit 0\n"
        result = interactive(command, script, environment, directory)
        assert result.returncode == 0, result.stderr
        records = []
        for slot in range(32):
            path = runtime / f"templaros-garden/shell-owned.{slot}"
            if path.exists():
                assert path.stat().st_mode & 0o077 == 0, path
                fields = path.read_text().rstrip("\n").split("\t")
                assert len(fields) == 9 and fields[0] == "1", fields
                records.append(fields)
        assert records, result.stderr
        serialized = repr(records)
        assert "PRIVATE_SENTINEL" not in serialized, serialized
        assert "VISIBLE_SENTINEL" in serialized, serialized
        assert any(fields[5] == "1 0" for fields in records), records
        expected = "0" if shell == "fish" else "1"
        assert any(fields[4] == expected and fields[5] == "1 0" for fields in records), records
        trace = (directory / "trace").read_text()
        assert "garden observe terminal-entered" in trace, trace
        print(f"{shell}: private journal, leading-space privacy, pipeline vector/exit, cwd hook preserved")


def kitty_interop(integration: Path) -> None:
    """Exercise the installed script's real ENV injection, prompt lists and PS0."""
    if not integration.is_file():
        raise SystemExit(f"unavailable: installed Kitty Bash integration at {integration}")
    binary = shutil.which("bash")
    if binary is None:
        raise SystemExit("unavailable: bash is not installed")
    for injection in ("1", "posix"):
        for array in (False, True):
            for hooks_first in (False, True):
                with tempfile.TemporaryDirectory(prefix="templaros-kitty-hook-owned-") as temporary:
                    directory = Path(temporary)
                    runtime = directory / "runtime"
                    runtime.mkdir(mode=0o700)
                    (runtime / "templaros-garden").mkdir(mode=0o700)
                    # Prevent Kitty injection from sourcing the host's system rc.
                    etc = directory / "etc"
                    etc.mkdir()
                    recorder = directory / "templar"
                    recorder.write_text("#!/bin/sh\ncase \"$1:$2\" in\n garden:journal-init) printf '%s/templaros-garden/shell-owned\\n' \"$XDG_RUNTIME_DIR\";;\n *) printf '%s\\n' \"$*\" >> \"$TRACE_FILE\";;\nesac\n")
                    recorder.chmod(0o700)
                    trace_path = directory / "trace"
                    environment = dict(os.environ, HOME=str(directory),
                                       XDG_CONFIG_HOME=str(directory / "config"),
                                       XDG_RUNTIME_DIR=str(runtime), XDG_CURRENT_DESKTOP="TemplarOS",
                                       PATH=f"{directory}:/usr/bin:/bin", TRACE_FILE=str(trace_path),
                                       TEMPLAR_NOTIFY_MIN_SECONDS="999999",
                                       TEMPLAR_GARDEN_FULL_COMMANDS="1", HISTFILE="/dev/null",
                                       TERM="xterm-kitty", COLUMNS="100", LINES="30",
                                       ENV=str(integration), KITTY_SHELL_INTEGRATION="enabled",
                                       KITTY_BASH_INJECT=injection, KITTY_PID="owned",
                                       KITTY_BASH_ETC_LOCATION=str(etc))
                    # Do not inherit clone/startup commands or remote integration inputs.
                    for key in ("KITTY_IS_CLONE_LAUNCH", "KITTY_SI_RUN_COMMAND_AT_STARTUP",
                                "SSH_KITTEN_KITTY_DIR", "KITTY_BASH_UNEXPORT_HISTFILE"):
                        environment.pop(key, None)
                    hooks = (f"source {shlex.quote(str(HOOKS / '60-templaros-garden.sh'))}\n" +
                             f"source {shlex.quote(str(HOOKS / '61-templaros-completion.sh'))}\n")
                    previous_debug = ('printf \'previous-debug:%s:%s:%s\\n\' "single\'quote" '
                                      '"$?" "$BASH_COMMAND" >> "$TRACE_FILE"; '
                                      '[[ "$BASH_COMMAND" != "printf \'SKIPPED_EXECUTION\\n\'" ]]')
                    prompt_command = ("PROMPT_COMMAND=(__user_prompt 'builtin : && builtin :')\n"
                                      if array else
                                      "PROMPT_COMMAND='__user_prompt; builtin : && builtin :'\n")
                    rc = directory / "rc"
                    rc.write_text(
                        "HISTCONTROL=ignorespace\nset -o pipefail\n"
                        "__user_prompt() { local status=$?; printf 'user-prompt:%s\\n' "
                        "\"$status\" >> \"$TRACE_FILE\"; return \"$status\"; }\n"
                        "__owned_prompt_decoration() { printf 'decorated:'; }\n"
                        "PS1='$(__owned_prompt_decoration)owned-ready> '\n"
                        "PS0='original-ps0:'\n" + prompt_command +
                        "trap " + shlex.quote(previous_debug) + " DEBUG\n" +
                        # Inherited DEBUG traps also exercise PS0/PS1 substitutions and
                        # Kitty/user function bodies, not only top-level prompt elements.
                        ("set -o functrace\nshopt -s extdebug\n" if not hooks_first else "") +
                        (hooks if hooks_first else ""))
                    environment["KITTY_BASH_RCFILE"] = str(rc)
                    environment["KITTY_BASH_POSIX_ENV"] = str(rc)

                    def inspect(expected):
                        records = []
                        for slot in range(32):
                            path = runtime / f"templaros-garden/shell-owned.{slot}"
                            if path.exists():
                                assert path.stat().st_mode & 0o077 == 0, path
                                fields = path.read_text().rstrip("\n").split("\t")
                                assert len(fields) == 9 and fields[0] == "1", fields
                                records.append(fields)
                        records.sort(key=lambda fields: int(fields[1]))
                        assert len(records) == expected, (injection, array, hooks_first, records)
                        assert "PRIVATE_EXECUTED" not in repr(records), records
                        assert not any(fields[7] == "command" for fields in records), records
                        if expected >= 2:
                            assert records[1][4:6] == ["1", "1 0"], records
                        return records

                    steps = []
                    if not hooks_first:
                        steps.append((hooks.replace("\n", "; ") + "\n", lambda: inspect(0)))
                    steps.append((None, lambda: inspect(0)))
                    steps.extend(("\n", lambda: inspect(0)) for _ in range(3))
                    steps.extend([
                        ("printf 'REAL_EXECUTED\\n'\n", lambda: inspect(1)),
                        ("false | true\n", lambda: inspect(2)),
                        (" printf 'PRIVATE_EXECUTED\\n'\n", lambda: inspect(2)),
                        ("printf 'AFTER_PRIVATE\\n'\n", lambda: inspect(3)),
                    ])
                    steps.extend(("\n", lambda: inspect(3)) for _ in range(3))
                    steps.append(("false; printf 'AFTER_FAILURE\\n'\n", lambda: inspect(4)))
                    if not hooks_first:
                        steps.append(("printf 'SKIPPED_EXECUTION\\n'\n", None))
                    result = interactive([binary, "--posix", "-i"], steps, environment,
                                         directory, prompt=b"owned-ready> ")
                    assert result.returncode == 0, result.stderr
                    assert "REAL_EXECUTED\r\n" in result.stderr, result.stderr
                    assert "PRIVATE_EXECUTED\r\n" in result.stderr, result.stderr
                    assert "AFTER_PRIVATE\r\n" in result.stderr, result.stderr
                    assert "AFTER_FAILURE\r\n" in result.stderr, result.stderr
                    assert "original-ps0:" in result.stderr, result.stderr
                    if not hooks_first:
                        assert "SKIPPED_EXECUTION\r\n" not in result.stderr, result.stderr
                    # Kitty really ran: prompt, command and CWD protocol marks survive.
                    for marker in ("\x1b]133;A", "\x1b]133;C;", "\x1b]7;kitty-shell-cwd://"):
                        assert marker in result.stderr, (marker, result.stderr)
                    trace = trace_path.read_text()
                    assert "garden observe terminal-entered" in trace, trace
                    assert "user-prompt:1" in trace, trace
                    assert "previous-debug:single'quote:0:printf 'REAL_EXECUTED\\n'" in trace, trace
                    assert "previous-debug:single'quote:1:printf 'AFTER_FAILURE\\n'" in trace, trace
    print("bash/Kitty: actual injection, idle/compound prompts, PS0, prior DEBUG/extdebug, command execution, privacy and pipeline preserved")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--shell", choices=("bash", "zsh", "fish"), required=True)
    parser.add_argument("--kitty-bash", type=Path,
                        default=Path("/usr/lib/kitty/shell-integration/bash/kitty.bash"))
    arguments = parser.parse_args()
    smoke(arguments.shell)
    if arguments.shell == "bash":
        smoke("bash", histcontrol="")
        smoke("bash", histcontrol="ignoredups")
        kitty_interop(arguments.kitty_bash)
