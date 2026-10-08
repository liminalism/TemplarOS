# portals/ — `xdg-desktop-portal-templar`

The single TemplarOS portal backend (`org.freedesktop.impl.portal.desktop.templaros`). It serves the four
interfaces `templaros.portal` claims, and only those: FileChooser → `templar-filed` (shared last-folder
policy, filters, hidden files), AppChooser → `Associations1` + `AppIdentity1`, Background →
explicit interactive consent, Settings → `Settings1`.

**There is no OpenURI backend, and there is nothing to write one against.**
`org.freedesktop.impl.portal.OpenURI` does not exist: `xdg-desktop-portal` (1.20.3 on the Ubuntu
base) implements `org.freedesktop.portal.OpenURI` itself. It finds the handler through
GAppInfo/`mimeapps.list` — the file `templar-associationd` generates — asks the selected AppChooser
when it has to ask, and launches the chosen desktop entry in its own scope. The vendor default
AppChooser is GTK; the native backend remains an explicitly selectable alternative.
Templar defaults reach the frontend through exported `mimeapps.list`, not a new portal interface. Two consequences:
the launch does not go through `Launcher1`, so a file opened this way is not started into a TemplarOS
cgroup; and the export is regenerated on every association change but not at session start, so a
home whose `mimeapps.list` was never written can disagree with `Associations1` until something
changes a default.

## Supported selection and packaging

`XDG_CURRENT_DESKTOP=TemplarOS` selects the vendor file
`/usr/share/xdg-desktop-portal/templaros-portals.conf`:

- FileChooser: `lxqt;gtk`, with LXQt preferred. This is descriptor-selection preference, **not**
  automatic recovery after a selected backend crashes.
- AppChooser: GTK; Settings: `templaros;gtk`; other advertised GTK interfaces use GTK.
- Background, ScreenCast and Screenshot: `none`. GTK does not advertise Background; no packaged
  capture backend has a proven Templar compositor contract. Native Background is opt-in until
  its installed graphical consent gate passes. No existence-based permission grants remain.

The installed GTK and LXQt descriptors advertise the selected interfaces; LXQt advertises
FileChooser only. The image selection and portal Debian dependencies include
`xdg-desktop-portal`, `xdg-desktop-portal-lxqt`, `xdg-desktop-portal-gtk`, and `qt6-wayland`.
Resolve Qt/KF6/libfm-qt transitive closure through the configured Ubuntu snapshot, not a
developer-machine installation. Keep the real desktop identity; do not set global
`GTK_USE_PORTAL=1` or impersonate LXQt.

The owner deferred the native PixelKit chooser after visual review on 2026-10-07.
Passing native protocol or browser tests does not authorize switching the vendor default;
PCManFM-Qt and the LXQt policykit agent remain selected independently.

There is no source implementation of the old claimed sessiond user-config regeneration.
This package installs **only vendor defaults**, never overwrites user `portals.conf` or
desktop-specific overrides, and cannot safely migrate an unmarked preexisting file.
User overrides therefore remain authoritative and may select an unavailable backend.

The compositor's installed-session activation path publishes the **actual** listener basename
to both the systemd user manager and ordinary D-Bus activation before READY. The launcher
removes stale `templaros/compositor-display.json`, reads the fresh schema/PID/socket after
Type=notify readiness, and uses it for PAM-session desktop helpers. Selected old portal
processes are retired at login so they cannot retain the previous desktop's display.
Direct/private compositor runs must not use `--session-activation` or call the real user manager.

## Native host and honest limits

One persistent PixelKit event loop owns the process main thread. D-Bus work sends models through
a channel and `Waker`; each request owns a separate window and one-shot response. Closing one
dialog leaves others and the idle host alive. `Request.Close`, caller disappearance and the
five-minute deadline drop the pending UI future and wake/retire exactly that window.
Cancel/Escape/window close reply **1**; missing/failed display, response-channel failure and timeout
reply **2** with a diagnostic `error` result. Failure never grants Background permission.

Request-scoped activation tokens are passed to window attributes, never raced through
global environment. The native backend supports presentation-only xdg-foreign v2 parenting
and an isolated X11 dialog helper; backend death retires that helper. These development
repairs do not imply visual acceptance or native-default promotion.

The supported LXQt 1.3.0 backend handles `x11:` parent identifiers but ignores `wayland:`
identifiers in its upstream `Utils::setParentWindow`. Real Firefox and Chromium Wayland
dialogs appeared and accepted input in the owned runtime, but had no transient-parent
relationship. Do not claim parent-constrained stacking or fullscreen parenting on that
version. Native `UpdateChoices` remains explicitly NotSupported. Fullscreen/output
movement and installed-session activation require separate evidence.

Native dialogs use the existing `Focus`, controls, theme changes and optional AccessKit/AT-SPI
adapter. Tab/Shift-Tab traverses controls; arrows navigate choices, filters and the full scrolling
file list; Enter operates the focused control; Escape cancels. Full committed Unicode and IME
preedit/commit reach the filename field, and its physical caret anchors the candidate window.
Async clipboard failures are shown inline. This is not a claim of complete screen-reader
text-selection/editing or complete Unicode shaping support.

## Owner verification (not executed by this implementation lane)

After integrating both repositories, run the portal, launcher and PixelKit tests and refresh
both Cargo locks. Required tools for these smoke commands: `xvfb-run`, `dbus-run-session`,
`gdbus`, `xdotool`; native Wayland clipboard also needs the nested compositor's
`zwlr_data_control_manager_v1` (already bound in source). No live user-unit or deployment commands.

Export `PORTAL_BIN` as the built absolute `xdg-desktop-portal-templar` path. This private-bus,
private-display script opens and cancels three **concurrent pairs**, then a fresh request:

```sh
t=$(mktemp -d)
mkdir -m 700 "$t/home" "$t/runtime" "$t/config"
env -u WAYLAND_DISPLAY -u DBUS_SESSION_BUS_ADDRESS \
  HOME="$t/home" XDG_RUNTIME_DIR="$t/runtime" XDG_CONFIG_HOME="$t/config" \
  XDG_CURRENT_DESKTOP=TemplarOS XDG_SESSION_TYPE=x11 \
  timeout 60s xvfb-run -a dbus-run-session -- sh -eu -c '
    "$PORTAL_BIN" & portal=$!
    trap "kill $portal 2>/dev/null || :; wait $portal 2>/dev/null || :" EXIT
    n=0
    until gdbus introspect --session --dest org.freedesktop.impl.portal.desktop.templaros \
      --object-path /org/freedesktop/portal/desktop >/dev/null 2>&1; do
      n=$((n+1)); test "$n" -lt 100; sleep .05
    done
    for round in 1 2 3; do
      for which in a b; do
        gdbus call --session --dest org.freedesktop.impl.portal.desktop.templaros \
          --object-path /org/freedesktop/portal/desktop \
          --method org.freedesktop.impl.portal.FileChooser.OpenFile \
          /org/freedesktop/portal/desktop/request/smoke/${which}${round} \
          org.example.Smoke "" "Smoke ${which}${round}" "{}" &
        if test "$which" = a; then first=$!; else second=$!; fi
      done
      for which in a b; do
        xdotool search --sync --pid "$portal" --name "Smoke ${which}${round}" >/dev/null
        gdbus call --session --dest org.freedesktop.impl.portal.desktop.templaros \
          --object-path /org/freedesktop/portal/desktop/request/smoke/${which}${round} \
          --method org.freedesktop.impl.portal.Request.Close
      done
      wait "$first"; wait "$second"
    done
    gdbus call --session --dest org.freedesktop.impl.portal.desktop.templaros \
      --object-path /org/freedesktop/portal/desktop \
      --method org.freedesktop.impl.portal.AppChooser.ChooseApplication \
      /org/freedesktop/portal/desktop/request/smoke/app org.example.Smoke "" \
      "[\"org.example.One\", \"org.example.Two\"]" "{}" & caller=$!
    xdotool search --sync --pid "$portal" --name "Open with" windowfocus key Escape
    wait "$caller"
  '
rm -rf "$t"
```

Every completed interaction must return code 1, with no task/display error masquerading as
cancel. Repeat with SaveFile/SaveFiles and select a UTF-8 fixture; run NotifyBackground and
exercise both Forbid/Allow and cancellation. Kill only a recorded private caller PID while its
dialog is open and require that window to disappear; restart only the owned backend PID and
repeat. In another private bus, unset both display variables and require code 2. Killing the
owned private display while a request is open must yield an infrastructure failure/disconnect,
not a successful cancel. Use explicit time bounds around smoke commands so a missing dialog
cannot hang a verification job.

For native Wayland, run the built compositor directly under the private Xvfb/bus with
`--backend winit --socket-name portal-smoke`, **without** `--session-activation`; clients use
`WAYLAND_DISPLAY=portal-smoke` and unset DISPLAY. Do not use a helper which contacts a real
systemd user manager. The compositor requires its production fonts and graphics dependencies.
Start `clipboard_peer set \"ชื่อ😀.txt\" 30000` on that socket, wait for its READY output, and run
`clipboard_peer get` as a separate process. Repeat the reverse direction with browser/Settings,
and with X11 under Xvfb. `clipboard_peer` is the permanent example in sibling PixelKit:

```sh
cargo build --manifest-path ../pixelkit/Cargo.toml -p pixelkit-shell --example clipboard_peer
# Export COMPOSITOR_BIN and CLIPBOARD_PEER_BIN as built absolute paths.
# Run inside the SAME private Xvfb, dbus-run-session and XDG_RUNTIME_DIR;
# DISPLAY must name the private Xvfb, never the live desktop.
timeout 60s sh -eu <<'WAYLAND_SMOKE'
env -u WAYLAND_DISPLAY "$COMPOSITOR_BIN" --backend winit --socket-name portal-smoke &
comp=$!
writer=
ready=$(mktemp)
trap 'test -z "$writer" || kill "$writer" 2>/dev/null || :; kill "$comp" 2>/dev/null || :; rm -f "$ready"' EXIT
n=0
while ! test -S "$XDG_RUNTIME_DIR/portal-smoke"; do
  n=$((n+1)); test "$n" -lt 100; sleep .05
done
env -u DISPLAY WAYLAND_DISPLAY=portal-smoke "$CLIPBOARD_PEER_BIN" set "ชื่อ😀.txt" 30000 > "$ready" &
writer=$!
n=0
while ! test -s "$ready"; do
  kill -0 "$writer"; n=$((n+1)); test "$n" -lt 100; sleep .05
done
IFS= read -r event < "$ready"
test "$event" = READY
actual=$(env -u DISPLAY WAYLAND_DISPLAY=portal-smoke "$CLIPBOARD_PEER_BIN" get)
test "$actual" = "ชื่อ😀.txt"
WAYLAND_SMOKE
```

The installed S01 gate additionally requires Firefox (declared Flatpak) and Chromium in every
advertised release packaging format, the actual frontend/LXQt backend and document portal,
and a disposable installed session or VM for systemd activation. Attach and read known local
fixture bytes (no remote upload of personal files), cancel, repeat at least three times, Save As,
and verify a missing/failing backend is diagnosable. Fullscreen parent, unplug/move outputs,
IME setup, Orca/AT-SPI and 1x/1.25x/1.5x/2x/mixed-output rendering are owner acceptance, not
automated claims. Keep LXQt the default; native promotion requires renewed owner approval.

## Native default-picker observations (2026-10-06)

The private native fixture in
`.agent/scratch/default-picker-uejt_2ej/observations.json` exercised the **selected LXQt**
backend through the real frontend, not the opt-in Templar FileChooser backend. The unmodified
vendor configuration was copied into an owned `XDG_CONFIG_DIRS` location with
`XDG_CURRENT_DESKTOP=TemplarOS`. The compositor's actual socket and private input listener
were ready before publishing display variables to the private D-Bus activation environment.
Neither `--session-activation` nor a live systemd user manager was used.

Observed providers: `xdg-desktop-portal` 1.21.1, LXQt 1.3.0, GTK 1.15.3, Qt Wayland 6.10.2.
Firefox 157.0 was installed from Flathub into the fixture's **owned user installation**;
the system installation initially lacked the declared Firefox ref. Chromium Flatpak
154.0.8037.57 and the extracted official Chrome 154 also ran as real Wayland clients.
All browser file inputs were opened by native clicks, never `setInputFiles`.

- Firefox and Chromium Flatpak each read the same 69 known local bytes through genuine
  document-portal exports at least three times, cancelled through the picker, and completed
  real Save As. Firefox's saved text matched the source byte-for-byte:
  `247cf94937b0254ce6b1d0abf1753b5d69887f6aa370099dab685e642a94894d`.
  Chromium's saved 578-byte local HTML also matched its source exactly.
- The activated owner was `org.freedesktop.impl.portal.desktop.lxqt`; its recorded environment
  contained the actual ready socket, `XDG_CURRENT_DESKTOP=TemplarOS`, and empty `DISPLAY`.
  Firefox and Chromium were identified by the frontend as their real Flatpak application IDs.
- Removing only the LXQt descriptor in a reversible private descriptor directory, retaining
  the exact vendor routing file, selected GTK and returned the same known bytes. Killing only
  the selected owned LXQt process during a request returned response **2** with the frontend
  diagnostic “Message recipient disconnected from message bus without replying”, not GTK
  runtime failover. Ordinary picker cancellation returned **1**.
- The inspected native screenshots, request traffic, provider environments, release-format
  facts and exact owned PID/process-group waits/reaps are retained alongside that JSON.

Private Flatpak fixtures need a short owned runtime path: a long path prevented the PKCS#11
helper's Unix socket from being created. Restarting only that owned helper after publishing the
short private runtime fixed the fixture prerequisite. Flatpak presents its conventional
`/run/user/1000/doc` inside the sandbox; this fixture additionally exposed **only its owned
document-portal mount** at the original private `/tmp` path (read-only for attachment, writable
for Save As). Without that mapping, a valid export produced a browser `NotFoundError`.
These fixture adjustments do not certify an installed session.

The file-type popup journey exposed a separate compositor focus/hit-testing defect: clicking
the real LXQt popup could raise Firefox over the still-pending chooser. The compositor now
resolves the actual popup surface to its owning toplevel before geometry-only press/chrome
handling. After rebuilding and restarting only the owned compositor/browser/backends, the
same **native host pointer** click at `(672, 672)` selected “Web Page, HTML only”, preserved
picker focus/stacking, and completed a 69-byte HTML Save As matching the known source exactly.
The corrected Firefox also repeated three document-export reads, cancelled and observed
response **2** after the selected owned backend failed. The reproduction and corrected
`xdg_popup` parent/configure traffic, inspected screenshots and snapshots are retained in
`popup-snapshots.json`, `popup-fixed-snapshots.json`, `popup-fixed-wayland-facts.log`,
`popup-fixed-save-result.json` and `popup-*.png` in the same artifact directory.

**Full S01 remains unsigned:** fresh installed-session/systemd activation and the physical
journey have not run. The available Chromium Snap launcher rejected the inherited outer scope
as “not a snap cgroup”; that format is not proven by the archive or Flatpak observations.
Fullscreen/output/IME/accessibility/scaling gates listed above remain separate owner acceptance.
Keep `FileChooser=lxqt;gtk` and Background/capture `none`; these observations do not authorize
switching the default to the custom native backend.

## Restored LXQt route observations (2026-10-07)

The owned runtime `.agent/scratch/lxqt-restored-l7k82pom/results.json` used the restored
vendor routing, real frontend and LXQt 1.3.0, private buses, and a software-rendered nested
compositor. Chromium Flatpak 154.0.8037.57 and Firefox Flatpak 157.0 each selected the same
40-byte local UTF-8 fixture three times, cancelled once, and saved byte-identical text
through the actual Save As dialog. No file-input injection or remote upload was used.
Screenshots and backend request logs are retained with the result.

Both browsers supplied Wayland parent handles, but LXQt did not create a transient-parent
relationship; the version-specific limitation above is observed, not a compositor parenting
claim. The fixture ran within an agent-owned 6-GiB memory-limited scope. No installed desktop
unit was restarted, and these results do not certify an installed package set.
