# Daily-driver capability matrix

What a normal development day on LegeOS actually needs, one row per journey, with the evidence that
exists **today** rather than the evidence we would like. Produced by the Developer Edition 0.1
campaign's daily-driver lane against `8b284c9`; contract C6, consumed by the package lane (D1) and
the hardware lane (D4).

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
| 1 | **Browser upload and download** — a web page asks for a file, or offers one to save | Sandboxed app → `org.freedesktop.portal.FileChooser` → our `impl.portal.FileChooser` → the Pixelkit dialog, starting in the folder `Files.Recents1.SuggestDirectory` names and recording where the person ended up | **automated**: `browser::tests::dialog_model_lists_all_entries`, `file_chooser::tests::filechooser_filters_apply`, and the five existing FileChooser tests. **manual**: the browser run in the checklist below | Firefox is declared as a Flatpak (`distro/seeds/selection.toml`, `browser` layer, `org.mozilla.firefox` from `flathub`) and emitted to `distro/seeds/flatpak-refs.list`; both consumers are written (`installer/provision.sh`, `distro/image/mkosi.postinst`). The honest state today is **declared, consumers written, not yet installed by the image** — shipping the list to `/usr/share/legeos/` still needs one asset line that no lane owns | — |
| 2 | **Open a downloaded file, or an external link** | `xdg-desktop-portal` implements `org.freedesktop.portal.OpenURI` **itself**: default handler from GAppInfo/`mimeapps.list`, our `impl.portal.AppChooser` when it must ask, then it launches the desktop entry in its own scope. `lege-associationd` generates that `mimeapps.list` | **automated**: `app_chooser::tests::openuri_resolves_default_handler` (the LegeOS default is recognised in the frontend's own spelling) and three existing AppChooser tests. **manual**: the checklist. Works, with the two identity gaps below | `xdg-utils`, `shared-mime-info` already seeded; `xdg-desktop-portal-gtk` added for Print/Email/Notification/DynamicLauncher, which this backend does not implement | — |
| 3 | **Flatpak portal selection and permissions** | `legeos.portal` claims exactly four interfaces for `UseIn=LegeOS`; `legeos-portals.conf` sets `[preferred] default=legeos` and pins ScreenCast/Screenshot to cosmic. sessiond regenerates the user's `portals.conf` from the `portal-backend` provider role | **manual** — the selection is a file-level fact until a Flatpak app runs against it. `xdg-desktop-portal-cosmic` stays seeded for ScreenCast/Screenshot only, with that reason recorded | Both settled by D1: the cosmic entry keeps a reason line; `xdg-desktop-portal-gtk` seeded as the fallback | — |
| 4 | **Clipboard between applications** | Shell- and compositor-owned, not a portal: `shell/lege-shell/src/data_device.rs` + `clipboard.rs` over the compositor's `DataDeviceState`. Real transport, not a stub | **automated elsewhere** — covered by the shell's own tests, cited here, not re-run by this lane. **manual**: copy/paste in the checklist | — | — |
| 5 | **Audio and video playback, hardware decode** | PipeWire/WirePlumber carry audio; nothing in the tree plays media | **blocked**, then manual. No player existed in any seed until this campaign | `mpv` (role `media-player`) plus `gstreamer1.0-plugins-{base,good,bad}`, `gstreamer1.0-libav`, `gstreamer1.0-pipewire` now declared by D1; VA-API set (`libva*`, `va-driver-all`, `intel-media-va-driver`, `vainfo`) came from D4's own request | `tests/hardware/hardware-video-decode.toml` (not executed). **Two cautions from D4**: its `vaapi` probe reports *which profiles exist*, never that a video decoded on the GPU; and the 59 profiles seen on the dev host come from the **non-free** iHD driver, while the seeds declare the free `intel-media-va-driver` — a seeded install may offer fewer. That caveat is now in the declaration's own reason line, so it can be cited rather than remembered |
| 6 | **Suspend/resume, brightness and audio keys** | `Power1`, `Backlight1` and `Audio1` exist; XF86 key routing is the compositor's | **manual** | — | `tests/hardware/suspend-resume.toml` and `tests/hardware/brightness-and-audio-keys.toml` (both not executed) |
| 7 | **Removable storage: mount and eject** | `Files.Volumes1` answers `InfoForPath`, `List` and `CheckReserve` — and nothing else | **blocked in code**: there is no mount or eject verb anywhere, so there is nothing for a person to drive. Recorded as blocked, not as untested | `udisks2` already seeded — the missing piece is a LegeOS surface over it, not a package | `tests/hardware/removable-storage.toml` (not executed, carrying the blocking reason) |
| 8 | **Secrets, SSH credentials, MIME defaults** | MIME defaults are real (`Associations1` + the `mimeapps.list` export). Secrets are not: `org.freedesktop.secrets` had **zero** implementations in the tree, and `legeos-portals.conf` names no `impl.portal.Secret` backend | MIME defaults **automated** (row 2's tests, plus associationd's own). Secrets **blocked** until the provider ships — a browser cannot store a password and SSH has no keyring | `gnome-keyring` now declared with role `secret-service` (D1) | — |
| 9 | **Camera and microphone** | No Camera portal; PipeWire is present | **blocked** — `org.freedesktop.impl.portal.Camera` has no implementation in the tree, so there is nothing for a person to drive. It stays unbuilt on purpose: camera is not in the reference workflow, and adding a portal to make this table look complete would be the wrong trade. "Blocked" rather than "deferred" because this table's own vocabulary reserves blocked for "it cannot be exercised at all yet — the code, the package or the provider is missing", which is the true statement; deferred described the decision, not the state | — | — |

Rows 5, 6 and 7 are D4's to execute; external outputs, which no row here covers, are already owned
by `tests/hardware/output-hotplug.toml`.

## What this lane changed, and what it deliberately did not

Fixed in the portal:

- The file dialog listed **ten entries per directory** and stopped. It now lists everything
  `lege-filed` returns (bounded at that service's 500, and the bound is shown when it is hit),
  scrolls, offers a hidden-file toggle, applies the request's `filters`/`current_filter`, seeds a
  save from `current_name`/`current_file`, and takes `SaveFiles`' `files` names into the folder the
  person chose. All of that is now a model with no display in it, so it is testable.
- AppChooser could not recognise its own defaults. The frontend offers **desktop file ids**;
  `Associations1` answers with **LegeOS canonical app ids**; the old comparison was string equality,
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
   desktop entry in its own scope, so the process does not land in a LegeOS cgroup and
   `AppIdentity1` cannot recognise it. LegeOS identity comes from the cgroup by design, so a file
   opened from a browser is, to the rest of the system, an anonymous process.
2. **The export is regenerated on change, not at start.** `lege-associationd` rewrites
   `mimeapps.list` after every association mutation (`crates/lege-associationd/src/dbus.rs`), and
   nothing writes it when a session begins. A fresh home — or one whose file was removed — can
   therefore disagree with `Associations1` until something changes a default. Both are recorded in
   the ledger; neither is fixed here, because both fixes live outside this lane's paths.

## User-run daily-driver checklist

None of this is run by an agent. Each step says what to do and what "it worked" looks like; if a
step cannot be run, record it as blocked with the reason rather than skipped.

**Before you start.** These steps use a nested session, which shares your user bus and refuses to
start if an installed LegeOS session is already running. Nothing here should be run on a machine
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
   with a file input and choose a file. Expect the LegeOS dialog (not a GTK one), starting in the
   folder you last used. Record which backend appeared — that is the portal-selection evidence for
   row 3.
4. **Browser download, then open it.** Download a file, then open it from the browser's downloads
   list. Expect: the save dialog offers the name the browser proposed and lets you change the
   folder; opening it afterwards launches the application `lege open <path>` names — compare the two
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
