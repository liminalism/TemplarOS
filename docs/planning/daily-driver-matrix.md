# Daily-driver capability matrix

This matrix retains the Developer Edition 0.1 campaign at `8b284c9`, contract C6, for
historical comparison. Its original table is not a current source-capability inventory:
the delivered baseline subsequently added login/fingerprint and UDisks automount work.
The dated bounded observations below supersede only the journeys they actually exercised.
Physical and installed-image acceptance remains the hardware lane's responsibility.

Three words are used precisely, and nothing is graded on intention:

| Word | Means |
|---|---|
| **automated** | a test in this repository covers it and passes; the row names the test |
| **manual** | it can be exercised, but only by a person on real hardware or a real session |
| **blocked** | it cannot be exercised at all yet — the code, the package or the provider is missing |

A printed plan, a dry run and a mocked portal are none of these three. No row below claims hardware
evidence: this lane owns no hardware and ran nothing on the reference laptop.

## The matrix

| # | Journey | How it works today | Evidence | Package/provider need (D1) | Manual case (D4) |
|---|---|---|---|---|---|
| 1 | **Browser upload and download** | Frontend FileChooser → LXQt (GTK descriptor alternative); the native PixelKit picker remains opt-in | **Observed native, private Wayland**: actual Firefox157/Chromium154 Flatpaks each completed three exact69-byte uploads, cancel and Save As through LXQt; descriptor removal selected GTK. LXQt loss returned error2, not pretend failover. Installed relogin and physical-session acceptance remain **manual, pending** | `xdg-desktop-portal-lxqt`, GTK and Qt Wayland are in the image selection and portal dependencies; Firefox is declared as `org.mozilla.firefox` from Flathub | `portals/README.md`; retained `.agent/scratch/default-picker-uejt_2ej/` |
| 2 | **Open a downloaded file or external link** | Frontend OpenURI owns MIME lookup and launch; GTK is vendor-default AppChooser. Native AppChooser remains an opt-in Associations1/AppIdentity1 consumer | **Observed native, isolated X11**: frontend OpenURI.OpenFile asked the actual native chooser and launched installed Kate for an owned Unicode file through its unmodified desktop entry. This did not create a Templar canonical instance; installed default route remains **manual, pending** | `xdg-utils`, `shared-mime-info`, GTK backend | Confirm actual desktop-file ID and downloaded-file journey |
| 3 | **Flatpak portal selection and permissions** | TemplarOS vendor configuration selects LXQt FileChooser, GTK AppChooser, native Settings; Background and unproven capture are explicitly unavailable. No user-config writer or proven generated-file migration exists | Owned descriptor/config routed the genuine frontend to native dialogs; native backend death, restart and unavailable display were exercised. Installed configuration precedence, systemd/socket activation and sandbox document access remain **manual, pending** | Preserve unmanaged user overrides; no global GTK_USE_PORTAL, LXQt identity impersonation or Cosmic assumption | Disposable installed session/VM; browser packaging matrix |
| 4 | **Clipboard between applications** | Shell/compositor selections plus PixelKit native Linux arboard ownership: X11 selections or Wayland wlr data-control. UI text fields consume async clipboard completion through their existing Waker | **Observed native, isolated X11**: two simultaneous portal windows copied Unicode; Chrome ↔ portal filename field preserved Unicode and an appended sentinel with native keyboard shortcuts, without clipboard helpers. Physical/native-Wayland, actual IME and screen-reader acceptance remain **manual, pending** | Current compositor binds `zwlr_data_control_manager_v1`; PixelKit uses arboard 3.6.1 with wayland-data-control. Compositors lacking a usable selection transport report unavailable | `clipboard_peer` and private X11/native-Wayland commands in `portals/README.md` |
| 5 | **Audio and video playback, hardware decode** | PipeWire/WirePlumber carry audio; nothing in the tree plays media | **blocked**, then manual. No player existed in any seed until this campaign | `mpv` (role `media-player`) plus `gstreamer1.0-plugins-{base,good,bad}`, `gstreamer1.0-libav`, `gstreamer1.0-pipewire` now declared by D1; VA-API set (`libva*`, `va-driver-all`, `intel-media-va-driver`, `vainfo`) came from D4's own request | `tests/hardware/hardware-video-decode.toml` (not executed). **Two cautions from D4**: its `vaapi` probe reports *which profiles exist*, never that a video decoded on the GPU; and the 59 profiles seen on the dev host come from the **non-free** iHD driver, while the seeds declare the free `intel-media-va-driver` — a seeded install may offer fewer. That caveat is now in the declaration's own reason line, so it can be cited rather than remembered |
| 6 | **Suspend/resume, brightness and audio keys** | `Power1`, `Backlight1` and `Audio1` exist; XF86 key routing is the compositor's | **manual** | — | `tests/hardware/suspend-resume.toml` and `tests/hardware/brightness-and-audio-keys.toml` (both not executed) |
| 7 | **Removable storage: mount and eject** | `Files.Volumes1` answers `InfoForPath`, `List` and `CheckReserve` — and nothing else | **blocked in code**: there is no mount or eject verb anywhere, so there is nothing for a person to drive. Recorded as blocked, not as untested | `udisks2` already seeded — the missing piece is a TemplarOS surface over it, not a package | `tests/hardware/removable-storage.toml` (not executed, carrying the blocking reason) |
| 8 | **Secrets, SSH credentials, MIME defaults** | MIME defaults are real (`Associations1` + the `mimeapps.list` export). Secrets are not: `org.freedesktop.secrets` had **zero** implementations in the tree, and `templaros-portals.conf` names no `impl.portal.Secret` backend | MIME defaults **automated** (row 2's tests, plus associationd's own). Secrets **blocked** until the provider ships — a browser cannot store a password and SSH has no keyring | `gnome-keyring` now declared with role `secret-service` (D1) | — |
| 9 | **Camera and microphone** | No Camera portal; PipeWire is present | **blocked** — `org.freedesktop.impl.portal.Camera` has no implementation in the tree, so there is nothing for a person to drive. It stays unbuilt on purpose: camera is not in the reference workflow, and adding a portal to make this table look complete would be the wrong trade. "Blocked" rather than "deferred" because this table's own vocabulary reserves blocked for "it cannot be exercised at all yet — the code, the package or the provider is missing", which is the true statement; deferred described the decision, not the state | — | — |

Rows 5, 6 and 7 are D4's to execute; external outputs, which no row here covers, are already owned
by `tests/hardware/output-hotplug.toml`.

## Bounded native portal observation — 2026-10-06

The native Ubuntu run used private HOME/XDG state, separately owned session/system buses, an
authenticated Xvfb display and only an owned portal descriptor/selector. No installed selector,
user unit, desktop session or default was changed. Chrome 154.0.8037.97 ran with an owned profile
and `--no-sandbox`; this is genuine native frontend/dialog/application interaction, not Flatpak
document access or physical/S19 acceptance.

- The installed frontend was `xdg-desktop-portal 1.21.1`. Private `XDG_DESKTOP_PORTAL_DIR` also
  relocated its selector lookup; read-only installed GLib schemas plus the memory GSettings backend
  were necessary with private data directories. No fabricated application ID was supplied.
- Native frontend OpenFile, SaveFile and SaveFiles returned selected URI results. An actual
  SaveFiles edit was ignored because its filenames belong to the request; the misleading editable
  field was removed, and post-fix keyboard selection plus still-editable SaveFile were exercised.
- Concurrent requests had independent windows. Close and caller departure removed only their
  request; duplicate handles were rejected. Backend death and unavailable display yielded response
  2, not success; restart accepted another request. An empty save filename showed an error and
  emitted no success before cancellation. Closed frontend requests emitted no Response signal.
  A real 300-second backend deadline returned response 2 with `portal request timed out`,
  removed its window and retired its Request object. Actual Xvfb disconnect also returned 2.
- Actual sessiond Settings1 changes reached native and frontend SettingChanged. Read/ReadAll
  exposed the standard typed appearance namespace; producer owner loss failed reads rather than
  returning a cached success, and notifications resumed after owner replacement.
- A private installed AT-SPI launcher/registry, ready before backend restart, exposed actual named
  portal buttons, checkbox, file entry and AppChooser radio item; keyboard Tab exported entry focus.
  The choice root remained a frame with MODAL=false, not evidence of physical modality or an Orca
  journey. A backend started before that accessibility producer did not register until restart.
- Retained observations, command arguments and visuals are under
  `.agent/scratch/portal-ervkjl1x/`: `chrome-native-upload-bytes.png`,
  `chrome-native-download-dialog.png`, `genuine-frontend-handler-launched.png`,
  `portal-to-chrome-native-clipboard.png` and `savefiles-directory-only-fixed.png`.
  The targeted native portal suite passed 22 tests. Mock-echo/implementation-only tests were
  deleted instead of being re-pinned as acceptance.

Limits remain explicit: Xvfb is virtual; foreign-parent import/output placement and physical
modality were not proven. The embedded portal font showed a missing CJK glyph even though
selected/clipboard Unicode bytes survived. Portal dialogs here contain no secret input, so this
run makes no secret-field exclusion claim. The managed LXQt picker remains the shipping default.

## Bounded native foreign-parent observation — 2026-10-06

This subsequent run closes the earlier run's **foreign-parent** gap, not its physical-seat or
shipping-selector gaps. Native `templar-comp --backend winit`, the native portal backend, genuine
installed `xdg-desktop-portal 1.21.1`, Chrome 154's native Wayland backend and a real X11 parent ran
inside owned Xvfb/private HOME/XDG/session/system/AT-SPI namespaces. `GTK_USE_PORTAL=1` was set only
on the owned Chrome child, never globally; no live desktop or managed LXQt selector was changed.

- Actual public `zxdg_exporter_v2`/`zxdg_importer_v2` traffic connected two separate-client
  toplevels to an **active 1280×800 fullscreen** parent. Native pixels and compositor readback
  showed both children above that parent, still assigned `Normal`, unverified, and **not focused**.
  Public discovery did not expose shell, recovery or session-lock authority to those clients.
- Import destruction detached only its child; reparenting did not resurrect the old relationship.
  Export destruction, exported `wl_surface` destruction and exporting-client death revoked every
  currently bound import. Importing-client death retired its child without retiring the parent.
  Bare non-toplevel export/child attempts and a self-cycle received actual protocol errors while
  the compositor remained alive.
- Genuine frontend requests produced separate request-owned native dialogs and truthful
  expired/malformed/absent-parent fallback notices. Closing a live parented request retired only
  that request; sibling requests survived. Export loss detached both sibling dialogs. Four
  cancellation responses had separate frontend paths and response code 1. Keyboard cancellation
  and fixture-transform-corrected motion/button cancellation traversed compositor input, not
  focus overrides or replacement response handlers; this is **not** physical-pointer acceptance.
- An actual Chrome file-input click, using the same explicit fixture coordinate workaround,
  produced a genuine frontend chooser whose live compositor parent was the browser's actual
  Wayland window. Native Escape retired it. On X11, actual `WM_TRANSIENT_FOR` property
  readback named the owned native parent; parent death removed the property and left the chooser
  reachable until `Request.Close`.
- The opt-in private native input producer was corrected to supply normalized
  `AbsolutePositionEvent` coordinates. The fixture then inverted the **advertised** nested output
  transform (`Flipped180`) when converting logical target points to physical canvas points.
  Production input/viewport code was not changed. Raw canvas `(842, 653)` produced actual seat
  `(842, 147)` on the 800-high nested output: an **observed production coordinate mismatch**,
  not a passing pointer gate. Failing-before evidence is retained under
  `parent-p_20jzav/`; parent/protocol proof uses the explicitly recorded workaround.

Retained native command arguments, exact fixture source records, compositor snapshots, protocol
outcomes, frontend response paths, genuine screenshots and pidfd cleanup observations are under
`.agent/scratch/native-parent-proof/` (`latest.path` identifies the completed final run).
The final supervised run reaped all 20 direct children and 12 adopted descendants; every owned
pidfd reported exit. Native compositor/backend builds passed; the WindowService and portal suites
passed 196 and 23 tests respectively, with subsequent targeted strengthened transient-parent and
private normalized-input regressions also passing. Those tests are supporting evidence, not a
substitute for the native journey.

Subsequent pointer correction separates the backend's absolute-input axes from the output's
framebuffer transform: physical backends retain the advertised transform; winit uses normal
displayed-canvas coordinates. A fresh native compositor's real host pointer moved to
`(842, 653)` and its seat reported exactly `(842, 653)` on a 1200×800 canvas. Private input
`motion 200 137` also reported `(200, 137)`, with no inverse-transform fixture workaround.
Three viewport regressions passed, including scaled/inset nested geometry. The owned
compositor exited and its adopted XWayland child was reaped. This follow-up corrects the
observed coordinate mismatch; it does not retroactively convert the earlier browser fixture
into physical-button or hardware acceptance.


Limits: Xvfb/winit remain virtual, Chrome used an owned `--no-sandbox` profile, and no physical
hotplug, installed sandbox grant, physical fullscreen/recovery, exclusive modality, Orca, full
CJK/emoji font coverage or shipping-default claim is made. Standard parent tokens confer a
relationship only: they do not transfer application identity, protected layers or activation
authority. Independent fallback is not a fabricated parent or a fabricated activation grant.

## What this lane changed, and what it deliberately did not

Fixed in the portal:

- The file dialog listed **ten entries per directory** and stopped. It now lists everything
  `templar-filed` returns (bounded at that service's 500, and the bound is shown when it is hit),
  scrolls, offers a hidden-file toggle, applies the request's `filters`/`current_filter`, seeds a
  save from `current_name`/`current_file`, and takes `SaveFiles`' `files` names into the folder the
  person chose. All of that is now a model with no display in it, so it is testable.
- AppChooser could not recognise its own defaults. The frontend offers **desktop file ids**;
  `Associations1` answers with **TemplarOS canonical app ids**; the old comparison was string equality,
  so the default never matched and a system that knew the answer asked the person anyway. The two
  are now mapped through `AppIdentity1.ListApplications`, and the chooser shows display names
  instead of ids.
- The portal now starts in both nested-session harnesses, so the journeys above can be run at all.

Deliberately not done:

- **No `org.freedesktop.impl.portal.OpenURI`.** It is not an interface the portal frontend defines
  or calls: `xdg-desktop-portal 1.20.3` knows 25 impl interfaces and OpenURI is not among them, no
  such XML is installed, and none of the six installed backends claims it. Writing one would have
  produced an object nothing would ever call, and a `.portal` file that claimed a capability the
  system does not route. The docs that said otherwise were corrected instead.
- No applications were added to fill rows out.

## Two identity gaps in row 2, worth carrying forward

1. **The launch bypasses `Launcher1`.** When the frontend opens a downloaded file it starts the
   desktop entry in its own scope, so the process does not land in a TemplarOS cgroup and
   `AppIdentity1` cannot recognise it. TemplarOS identity comes from the cgroup by design, so a file
   opened from a browser is, to the rest of the system, an anonymous process.
2. **The export is regenerated on change, not at start.** `templar-associationd` rewrites
   `mimeapps.list` after every association mutation (`crates/templar-associationd/src/dbus.rs`), and
   nothing writes it when a session begins. A fresh home — or one whose file was removed — can
   therefore disagree with `Associations1` until something changes a default. Both are recorded in
   the ledger; neither is fixed here, because both fixes live outside this lane's paths.

## Bounded canonical Settings and source-package observations — 2026-10-06

Actual native Settings and its CLI use one 17-page inventory with ordinary devices,
appearance and Files before advanced pages; internal view discriminants do not determine
navigation order. Existing aliases and exact search labels resolve the same typed destination,
including explicit `volume`/`audio` precedence over System's volume search label.
An owned X11 run exposed wheel-down failing to reveal advanced navigation rows: the shared
PixelKit list now follows the existing scrollable wheel convention. Actual post-fix wheel
input revealed Resources, History and Hardware. The Files GUI and `--print --page index`
both showed the actual watched root at generation 5 with seven entries. Missing private
audio/input/display owners remained explicitly unavailable, not fabricated applied values.
An external CLI write changed the owned Homegarden limit from 64 to 32; the already-open
native GUI showed 32 and the private setting was restored to 64 afterward. The compact
screenshots and observations are retained in `.agent/scratch/native-ui-proof-20261006/`.

The unexpected new LiminalView image default now has a real selected Debian package and
content-addressed local source closure: five owning source roots, 334 locked dependencies,
vendored registry inputs and embedded fonts. Rootless native build from that archive with an
empty Cargo home produced the real executable and generated desktop. The extracted package
actually rendered the owned PNG in a private native window. Its supported-format command
advertised 22 format entries/26 MIME types; this is not a claim that every decoder was exercised.
No release Git origin or pin was invented for the non-Git local viewer; the complete archive
is the local reproduction input. No installed desktop, application, unit or image was changed.

## Real disposable native deployment observations — 2026-10-06

An unprivileged QEMU TCG/OVMF guest used an actual GPT disk, Btrfs subvolumes,
FAT ESP, Ubuntu kernel, systemd, dpkg, APT and a disposable signed archive. No host
mount, host-root privilege, network, live user bus or installed desktop was changed.
The real hostd factory staged a package-2.0 candidate from package-1.0 known-good,
then actual firmware selected the candidate entry. Boot-entry selection uses the
stable `templaros-{id}.conf` ID; systemd-boot's `+tries` suffix belongs to the on-disk
entry filename, not its selectable identity.

Fresh native hostd in the guest's runtime initramfs overlay resumed persisted
`AwaitingReboot` as `Trial` only when the actual kernel deployment ID matched.
Selecting an entry alone no longer records a trial boot. The candidate executable
and dpkg database both reported 2.0; system and dpkg writes failed with the expected
EROFS. Native `Deployments1.Rollback` selected known-good, and a subsequent actual
firmware boot restored executable and dpkg 1.0. Original Home/profile hashes,
candidate-created documents and shared runtime-state data survived; root stayed
read-only. Sealed roots, package databases, journal and EFI selection were not
rewritten by the runtime overlay.

A separate guest transaction was interrupted at its fsynced `Staging` journal by
killing only the owned guest hostd fixture service. After poweroff, unchanged
firmware selection booted known-good; the actual journal still had no candidate
and remained `Staging`. Executable/dpkg 1.0, read-only root, Home/profile hashes,
the prior candidate-created document and shared-state data were preserved. No
incomplete candidate was selected, and no journal, EFI default or health marker
was manufactured to obtain this outcome.

That guest also exposed early bus-name publication: the first host API could time
out before interfaces existed. Hostd now claims its name after recovery and all
registrations, following filed/sessiond. A fresh actual native daemon on a private
bus answered its first deployment API after name ownership within a one-second
deadline; exact children and disposable state were removed afterward.

These observations cover the minimal real deployment/rollback path, not a fresh
installed TemplarOS desktop, full promotion-health chain, shared-state schema
migration, Secure Boot, PAM/fingerprint or physical GPU/display daily-driver
acceptance. Those remain unsigned release gates.

## User-run daily-driver checklist

None of this is run by an agent. Each step says what to do and what "it worked" looks like; if a
step cannot be run, record it as blocked with the reason rather than skipped.

**Before you start.** These steps use a nested session, which shares your user bus and refuses to
start if an installed TemplarOS session is already running. Nothing here should be run on a machine
mid-work.

1. **Bring up the nested session.**
   `scripts/nested-session.sh start`, then `scripts/nested-session.sh status`.
   Expect `comp`, `sessiond`, `filed`, `portal` and `shell` all `running`. If `portal` says
   nothing started, check `.agent/scratch/nested-session/portal.log`: it declines when the
   freedesktop portal name already has an owner, which is intended.
2. **File dialog, by hand.** Launch anything that opens a file (`scripts/nested-session.sh launch
   <app-id>`). Expect: a folder with more than ten files shows all of them and scrolls; "Show
   hidden files" reveals dot-files; a filter menu appears when the application sent filters, and
   selecting one narrows files without hiding folders.
3. **Browser upload.** With Firefox installed from `distro/seeds/flatpak-refs.list` (today that means installing the ref by hand — see row 1), open a page
   with a file input and choose a file. Expect the managed LXQt picker by default; the native
   TemplarOS dialog requires an explicit owned opt-in selector. Record which backend appeared and
   whether the last-used folder was preserved — that is the portal-selection evidence for row 3.
4. **Browser download, then open it.** Download a file, then open it from the browser's downloads
   list. Expect: the save dialog offers the name the browser proposed and lets you change the
   folder; opening it afterwards launches the application `templar open <path>` names — compare the two
   directly, since they take different routes to the same answer.
5. **Clipboard.** Copy text in one application, paste it in another, both ways.
6. **Media.** Play an audio and a video file in `mpv`. Expect sound through PipeWire and smooth
   video. Hardware decode is D4's case, not this step: seeing `vainfo` list profiles is not
   evidence that a video decoded on the GPU.
7. **Suspend/resume** and **brightness/audio keys** — D4's `tests/hardware/*.toml` cases, run from
   its runner, not from here.
8. **Removable storage.** Insert a USB stick. Expect it to appear as a volume; note that no mount
   or eject action exists yet, so this step's honest outcome today is "seen, cannot be operated".
9. **Secrets.** Ask the browser to remember a password. Expect it to fail until `gnome-keyring` is
   installed and holds the `secret-service` role; with it, expect the password to survive a
   restart.
10. **Tear down.** `scripts/nested-session.sh stop`.

Everything the packages depend on is declared but not verified against an Ubuntu 26.04 archive:
D1's closure reports `resolution = "unavailable"` on this host, because no archive index exists
here. Treat every package name above as declared, not proven to exist.
