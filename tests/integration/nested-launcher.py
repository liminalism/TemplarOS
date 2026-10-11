#!/usr/bin/python3
"""Global launcher journey on the existing private bus and nested input socket.

Only the fixture's systemd manager is mocked. Catalogue, Index1, Associations1,
Launcher1, appd, the shell popup and compositor input/focus are production code.
The fake manager owns the private bus name and starts only this capture viewer;
it does not create real units or claim to prove cgroup accounting.
"""
import argparse
import base64
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
from urllib.parse import quote_from_bytes, unquote_to_bytes, urlsplit

QUERY = "launcherprobe"
APP = "desktop-import.org.templaros.LauncherProbe.desktop"
SCRIPT = Path(__file__).resolve()


def isolated(root):
    root = Path(root).resolve()
    assert os.environ.get("TEMPLAR_NESTED_ISOLATED") == "1", "private bus wrapper required"
    assert str(root).startswith("/tmp/templaros-nested."), "owned fixture root required"
    assert os.environ.get("XDG_RUNTIME_DIR") == str(root / "runtime"), "private runtime required"
    return root


def prepare(root):
    root = isolated(root)
    index = root / "probe-index"
    index.mkdir()
    target = os.fsencode(index) + b"/launcherprobe-\xff.txt"
    with open(target, "wb") as output:
        output.write(b"launcher native filename fixture\n")
    project = index / "project"
    project.mkdir()
    scoped = project / "projectprobe.txt"
    scoped.write_text("scoped fixture\n")
    (index / "projectprobe.txt").write_text("outside scope fixture\n")
    (root / "probe-target.json").write_text(json.dumps({"path_b64": base64.b64encode(target).decode(),
        "scoped_b64": base64.b64encode(os.fsencode(scoped)).decode(), "project": str(project)}))
    applications = root / "data/applications"
    applications.mkdir(exist_ok=True)
    # Desktop-entry quoting, with no shell interpretation or command substitutions.
    quote = lambda value: '"' + str(value).replace("\\", "\\\\").replace('"', '\\"') + '"'
    command = " ".join(map(quote, ["/usr/bin/python3", SCRIPT, "viewer", root]))
    (applications / "org.templaros.LauncherProbe.desktop").write_text(
        "[Desktop Entry]\nType=Application\nName=Launcherprobe Viewer\n"
        f"Exec={command} %U\nMimeType=text/plain;\nKeywords=launcherprobe;\n"
    )


def viewer(root, targets):
    root = isolated(root)
    record = {"app_id": os.environ.get("TEMPLAROS_APP_ID"), "targets": targets,
              "native_paths_b64": [base64.b64encode(unquote_to_bytes(urlsplit(uri).path)).decode()
                                   for uri in targets], "wayland": os.environ.get("WAYLAND_DISPLAY")}
    with (root / "probe-opened.jsonl").open("a") as output:
        output.write(json.dumps(record) + "\n")


MANAGER_XML = """<node><interface name="org.freedesktop.systemd1.Manager">
<method name="StartTransientUnit"><arg type="s" direction="in"/><arg type="s" direction="in"/>
<arg type="a(sv)" direction="in"/><arg type="a(sa(sv))" direction="in"/><arg type="o" direction="out"/></method>
<method name="GetUnit"><arg type="s" direction="in"/><arg type="o" direction="out"/></method>
<method name="ListUnitsByPatterns"><arg type="as" direction="in"/><arg type="as" direction="in"/>
<arg type="a(ssssssouso)" direction="out"/></method><property name="Version" type="s" access="read"/>
</interface></node>"""
SERVICE_XML = """<node><interface name="org.freedesktop.systemd1.Service">
<property name="ControlGroup" type="s" access="read"/>
</interface></node>"""


def manager(root):
    root = isolated(root)
    from gi.repository import Gio, GLib
    connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
    loop = GLib.MainLoop()
    units, children = {}, []
    service_info = Gio.DBusNodeInfo.new_for_xml(SERVICE_XML).interfaces[0]

    def method(connection, sender, path, interface, name, parameters, invocation):
        try:
            args = parameters.unpack()
            if name == "ListUnitsByPatterns":
                invocation.return_value(GLib.Variant("(a(ssssssouso))", ([],)))
                return
            if name == "GetUnit":
                invocation.return_value(GLib.Variant("(o)", (units[args[0]],)))
                return
            unit, mode, properties, auxiliary = args
            props = dict(properties)
            executable, argv, flags = props["ExecStartEx"][0]
            assert unit.startswith("app-templaros-") and mode == "fail" and not auxiliary
            assert executable == "/usr/bin/python3" and argv[1:4] == [str(SCRIPT), "viewer", str(root)]
            assert "no-env-expand" in flags, "production broker must preserve argv"
            environment = os.environ.copy()
            environment.update(assignment.split("=", 1) for assignment in props["Environment"])
            path = f"/org/freedesktop/systemd1/unit/probe{len(units) + 1}"
            # This is deliberately a mock manager property, not a real cgroup assertion.
            group = f"/fixture/app.slice/{unit}"
            connection.register_object(path, service_info, None,
                                       lambda *_: GLib.Variant("s", group), None)
            units[unit] = path
            with (root / "probe-broker.jsonl").open("a") as output:
                output.write(json.dumps({"unit": unit, "argv": argv}) + "\n")
            children.append(subprocess.Popen(argv, executable=executable, env=environment))
            invocation.return_value(GLib.Variant("(o)", ("/org/freedesktop/systemd1/job/1",)))
        except Exception as error:
            invocation.return_dbus_error("org.freedesktop.systemd1.Error.Fixture", str(error))

    connection.register_object("/org/freedesktop/systemd1",
        Gio.DBusNodeInfo.new_for_xml(MANAGER_XML).interfaces[0], method,
        lambda *_: GLib.Variant("s", "fixture"), None)
    owner = Gio.bus_own_name_on_connection(connection, "org.freedesktop.systemd1", Gio.BusNameOwnerFlags.NONE, None, None)
    # A normal Python signal handler can wait behind GLib's blocking poll.
    # This owned GLib source makes the wrapper's recorded-PID cleanup terminate.
    GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGTERM, lambda: (loop.quit(), False)[1])
    try:
        loop.run()
    finally:
        Gio.bus_unown_name(owner)
        for child in children:
            if child.poll() is None:
                child.terminate()
            child.wait(timeout=5)


def journey(root, socket, binary):
    root = isolated(root)
    sys.path.insert(0, str(SCRIPT.parents[2] / "scripts"))
    from importlib import import_module
    injector = import_module("nested-input").Injector(socket)
    targets = json.loads((root / "probe-target.json").read_text())
    target = targets["path_b64"]
    native = base64.b64decode(target)
    uri = "file://" + quote_from_bytes(native, safe="/")
    call = ["gdbus", "call", "--session", "--dest", "org.templaros.Session",
            "--object-path", "/org/templaros/Session/Associations", "--method"]
    subprocess.run(call + ["org.templaros.Associations1.SetMimeDefault", "text/plain", APP], check=True, capture_output=True, timeout=5)
    resolved = subprocess.run(call + ["org.templaros.Associations1.ResolveFileUriV1", uri, ""], check=True, capture_output=True, text=True, timeout=5)
    assert APP in resolved.stdout, resolved.stdout

    def index_ready(query, expected):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            result = subprocess.run([binary, "files", "index", "search", query, "--print0"], capture_output=True, timeout=5)
            if result.returncode == 0 and expected.issubset(set(result.stdout.split(b"\0"))):
                return
            time.sleep(0.05)
        raise AssertionError(f"Index1 never returned the fixture paths for {query!r}: {result.stdout!r}")

    index_ready(QUERY, {native})
    # Observe both ambiguous candidates before testing Search Here's scope. Finding
    # the root-level native file does not imply this subdirectory was indexed yet.
    index_ready("projectprobe", {base64.b64decode(targets["scoped_b64"]),
                                 os.fsencode(root / "probe-index/projectprobe.txt")})

    def until(predicate, description, stable_for=0.0, state_key=None):
        deadline = time.monotonic() + 10
        stable_since, previous_key, snapshot = None, None, None
        while time.monotonic() < deadline:
            snapshot = injector.command("snapshot")
            if predicate(snapshot):
                key = state_key(snapshot) if state_key else True
                if stable_since is None or key != previous_key:
                    stable_since, previous_key = time.monotonic(), key
                if time.monotonic() - stable_since >= stable_for:
                    return snapshot
            else:
                stable_since, previous_key = None, None
            time.sleep(0.02)
        raise AssertionError(f"timed out waiting for {description}: {snapshot}")

    def real_clients_ready(snapshot):
        clients = {window["title"]: window for window in snapshot["windows"]}
        if not {"Files", "Settings"}.issubset(clients):
            return False
        if any(clients[title]["parked"] or clients[title]["geometry"]["width"] <= 1
               or clients[title]["geometry"]["height"] <= 1 for title in ("Files", "Settings")):
            return False
        active = [window for window in snapshot["windows"] if window["active"]]
        return (len(active) == 1 and snapshot["keyboard"]
                and snapshot["keyboard"]["id"] == active[0]["id"] and not snapshot["home"])

    def client_state(snapshot):
        # Ignore transient tab/aim metadata while checking activation, committed
        # geometry and keyboard focus. Both fixture clients must already be mapped.
        return (snapshot["keyboard"]["id"], tuple(sorted(
            (window["id"], window["active"], window["maximized"],
             tuple(sorted(window["geometry"].items()))) for window in snapshot["windows"])))

    def launcher_focused(snapshot):
        return (snapshot["keyboard"] and snapshot["keyboard"]["title"] == "Templar Launcher"
                and any(window["title"] == "Templar Launcher" for window in snapshot["overlays"]))

    def launcher_dismissed(snapshot):
        return (not any(window["title"] == "Templar Launcher" for window in snapshot["overlays"])
                and snapshot["keyboard"] and snapshot["keyboard"]["id"] == active)

    def records():
        path = root / "probe-opened.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    try:
        # They start concurrently: Files can activate before Settings has committed
        # its first buffer. Wait for both, then choose a settled active client.
        before = until(real_clients_ready, "both mapped real clients with settled activation",
                       stable_for=0.25, state_key=client_state)
        active = next(w["id"] for w in before["windows"] if w["active"])
        injector.command("maximize", active)
        before = until(lambda s: real_clients_ready(s) and any(
            w["id"] == active and w["active"] and w["maximized"] for w in s["windows"]),
            "maximized real client with settled geometry and focus", stable_for=0.25, state_key=client_state)
        geometry = next(w["geometry"] for w in before["windows"] if w["id"] == active)
        injector.chord([125], 57)  # Super+Space, through production recovery bindings.
        injector.type(QUERY)       # No sleep/readiness poll: first keystrokes must survive mapping.
        injector.tap(15)           # Same typed text immediately switches into Files mode.
        popup = until(launcher_focused, "borrowed launcher focus")
        assert not popup["home"], popup
        client = next(w for w in popup["windows"] if w["id"] == active)
        assert client["active"] and client["geometry"] == geometry, popup
        launcher = next(w for w in popup["overlays"] if w["title"] == "Templar Launcher")
        assert popup["stacking"].index(launcher["id"]) < popup["stacking"].index(active), popup
        # Results arrive asynchronously after the popup has received the queued query/Tab.
        # Index freshness was already observed above; give its one IPC result frame to paint.
        time.sleep(0.35)
        injector.tap(28)
        until(launcher_dismissed, "launcher dismissal and focus restoration after Enter")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and not records():
            time.sleep(0.02)
        opened = records()
        assert len(opened) == 1 and opened[0]["native_paths_b64"] == [target], opened
        assert opened[0]["app_id"] == APP and opened[0]["wayland"] == "templaros-integration", opened
        final = injector.command("snapshot")
        assert not final["home"], final
        client = next(w for w in final["windows"] if w["id"] == active)
        assert client["active"] and client["geometry"] == geometry, final
        assert final["keyboard"] and final["keyboard"]["id"] == active, final
        assert len((root / "probe-broker.jsonl").read_text().splitlines()) == 1
        assert open(native, "rb").read() == b"launcher native filename fixture\n"

        # The real CLI/sessiond Lifecycle relay shares this popup, preserving explicit scope.
        search_here = [binary, "files", "search-here", targets["project"], "--query", "projectprobe"]
        subprocess.run(search_here, check=True, capture_output=True, timeout=5)
        scoped_popup = until(launcher_focused, "Search Here Files popup")
        assert not scoped_popup["home"], scoped_popup
        time.sleep(0.35)
        injector.tap(28)
        until(launcher_dismissed, "scoped file activation and focus restoration")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline and len(records()) < 2:
            time.sleep(0.02)
        assert len(records()) == 2 and records()[1]["native_paths_b64"] == [targets["scoped_b64"]], records()

        # A press delivered to the maximized client dismisses without committing a row.
        subprocess.run(search_here, check=True, capture_output=True, timeout=5)
        until(launcher_focused, "outside-click popup")
        injector.command("motion", geometry["x"] + 10, geometry["y"] + 10)
        injector.command("button", 272, "down")
        injector.command("button", 272, "up")
        final = until(launcher_dismissed, "outside-click dismissal and focus restoration")
        assert final["keyboard"] and final["keyboard"]["id"] == active, final
        client = next(w for w in final["windows"] if w["id"] == active)
        assert not final["home"] and client["active"] and client["geometry"] == geometry, final
        assert len(records()) == 2 and len((root / "probe-broker.jsonl").read_text().splitlines()) == 2
        assert open(native, "rb").read() == b"launcher native filename fixture\n"
        (root / "logs/launcher-journey.json").write_text(json.dumps({"before": before, "popup": popup, "scoped_popup": scoped_popup,
            "after": final, "opened": records()}, indent=2))
        print("nested launcher: ok (maximized client, immediate typing/Tab, native Index1/Associations/appd open, Search Here scope, outside dismissal, focus restored)")
    finally:
        injector.command("key", 125, "up")
        injector.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["prepare", "manager", "viewer", "journey"])
    parser.add_argument("root")
    parser.add_argument("extra", nargs="*")
    args = parser.parse_args()
    if args.mode == "journey":
        journey(args.root, *args.extra)
    elif args.mode == "viewer":
        viewer(args.root, args.extra)
    else:
        {"prepare": prepare, "manager": manager}[args.mode](args.root)


if __name__ == "__main__":
    main()
