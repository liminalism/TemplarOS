# TemplarOS: close the daily-driver gaps
## Source-grounded implementation plan — October 5, 2026

**Decision:** finish the integration of the architecture already present. Keep Ubuntu, systemd, Smithay, PixelKit, and the existing service boundaries. Use PCManFM-Qt and a compatible LXQt file-picker portal now. Make ordinary configuration coherent without turning this into another desktop rewrite.

This plan reviews the supplied October 5 TemplarOS and PixelKit archives and the earlier display alpha. It is not an applied patch, a successful build, or a hardware test. Findings marked **source-confirmed** describe the supplied snapshot; they are not diagnoses confirmed against the installed machine. [EVIDENCE.md](EVIDENCE.md) records exact paths, lines and input fingerprints. [BACKLOG.md](BACKLOG.md) turns the plan into bounded implementation slices and acceptance gates. New interface names below are proposals unless explicitly described as existing.

## 1. The important conclusions

Templar already has most of the right ownership boundaries: `templar-sessiond` is its policy daemon, `templar-comp` owns the display/input machinery, `templar-filed` handles heavy file work, `templar-hostd` owns Templar's privileged deployment operations, and `templar-resourced` is isolated from the ordinary policy loop. The Buninu lesson is therefore not “add a new control plane.” It is **finish the existing control plane and connect every visible action to an effective, observable operation**. [E01](EVIDENCE.md#e01)

| Finding in the supplied snapshot | Consequence | Decision |
|---|---|---|
| The native FileChooser runs PixelKit on a blocking worker, creating a default winit event loop for each dialog; failed task joins become cancellation. | A strong static explanation for clicking Attach and seeing nothing. | Delegate FileChooser to LXQt immediately; repair the shared native dialog host separately. |
| The DRM backend explicitly ignores non-primary GPUs. | A better settings window alone cannot activate a connector on an ignored GPU. | Multi-GPU scanout support is a prerequisite for the hybrid-laptop HDMI promise. |
| Settings has existing useful pages, but not the full ordinary network/audio/display/input navigation. | The user has to know implementation details and provider names. | Add a coherent settings route and capabilities-driven pages, preserving existing functions. |
| Audio controls the default sink's volume/mute, not complete device selection; power stores more policy than it enforces. | Some essential behavior is absent or could be misrepresented by a new switch. | Implement effects and read-back before exposing controls as working. |
| Manual locking now exists, but lock acknowledgment is sent before the protected-frame barrier. | Lock-before-suspend can race with visible unlocked content. | Fix the presentation acknowledgment before shipping automatic suspend behavior. |
| PixelKit's non-macOS clipboard is process-local. | Normal paste between a browser and a standalone PixelKit settings field is not supplied by that implementation. | Add real Linux clipboard integration as a toolkit requirement, not an application workaround. |
| PCManFM integration already exists, including provider selection and bookmark export. | There is no reason to build another file manager to unblock uploads. | Complete and test the integration; stop destructive whole-file bookmark export. |
| hostd's real staging path and the serialized boot subvolume disagree with the declared layout. | A “versioned deployment” claim is premature even before read-only-root work. | Fix and test mounted-path versus subvolume identity first. |

Evidence: [E02–E16](EVIDENCE.md), especially the portal call chain in E03/E04, the GPU guard in E06, the lock barrier in E11, and deployment wiring in E15. Winit documents a main-thread, once-per-application event-loop constraint; the current caller does neither. This is a concrete code problem, although installed logs are still needed to establish which failure the browser actually encountered. [U1]

## 2. Three user-facing layers, without three new systems

The proposed **Work / Applications / System** presentation and the existing **system / state / work** storage proposal answer different questions. Keep both, with an explicit mapping rather than forcing a one-to-one match.

| User-facing layer | What the user sees | Authority and persistence |
|---|---|---|
| **Work** | Projects, folders, documents, meaningful recent activity, work-linked window cards, return-to-work actions. | Homegarden and Projects APIs describe context. filed indexes and performs Templar-requested file work. Documents and application profiles remain owned by their user/application. |
| **Applications and workloads** | Installed application variant, open instances, permissions, background activity, resource policy, defaults and installation source. | Existing identity, launch, association and background APIs in sessiond; resourced enforces its supported resource policy. Application records and preferences are state. Executable files belong to their actual package/deployment or application installation. |
| **System** | Current deployment, available update, supported hardware state, restart, recovery, known-good version. | hostd coordinates Templar's privileged writes; Ubuntu services keep their existing roles. Runtime state is not indiscriminately rolled back with OS binaries. |

“Homegarden understands app state” must mean it can describe and restore supported context, not that it copies or interprets every application's internal database. Show **Reopen** when that is all an application supports, and **Restore session** only when a checkpoint contract actually exists. A browser profile is not a disposable cache simply because it is stored below a hidden directory.

Keep the current process model. Add Network/BlueZ/settings adapters as bounded modules behind sessiond's existing API framework; do not create six new permanent Templar daemons. Existing external services such as NetworkManager, BlueZ, PipeWire/WirePlumber, UDisks and logind are not displaced by “one Templar privileged service.” `templar-hostd` remains the one privileged **Templar-owned** service, not an unnecessary proxy for every authorized upstream call.

Keep `templar-filed` off the policy/render thread, and keep `templar-resourced` free of UI/database dependencies. Do not move graphics modesetting into sessiond, and do not embed winit in the compositor render core. E01 and E13 document why those separations already exist.

### A common operational contract

Every ordinary settings domain should expose an immutable snapshot, supported capabilities, a revision, and changes. A submitted operation has an ID and a final outcome: applied, rejected, canceled, timed out, or temporarily unavailable. **Desired value, applied value and saved preference are distinct.** An accepted message is not proof that hardware changed.

Use existing `templar-types`, `templar-client`, `interfaces/dbus/`, and schema conventions. Make additions backward compatible. A UI row needs enough information to say “Not supported by this device,” “Permission required,” or “Temporarily unavailable,” rather than silently doing nothing. Propagate external changes, not just changes made through Templar.

Potential new domains are `Network1`, `Bluetooth1`, `Displays1`, and `Input1`; their exact signatures must be added to the checked interface inventory. Extend existing `Audio1`, `Power1`, `Backlight1`, `Battery1` and `Settings1` where their scope fits. Do not create competing APIs for the same effect.

## 3. First delivery: working browser attachment dialogs

### 3.1 Separate the file manager from the file chooser

“PCMIN” is taken to mean **PCManFM-Qt**, which is already selected in Templar's package/provider work. Use it for browsing, file operations and opening folders. Use **xdg-desktop-portal-lxqt** for browser Open/Save dialogs. The LXQt backend uses Qt/KF6/libfm-qt; it is the matching file-dialog implementation, not a request to run `pcmanfm-qt` as an arbitrary modal window. [E14, E24; U2, U3]

The intended browser path is:

```text
Browser requests a file
    -> public xdg-desktop-portal FileChooser
    -> selected LXQt backend
    -> visible, parented chooser
    -> response containing selected URI(s)
    -> portal grants access where required
    -> browser reads the selected file
```

A chooser appearing is only half the test. The browser must receive and read the bytes, including for supported sandboxed packaging. The portal frontend owns the document-access grant mechanism; do not replace it with hand-built host paths or broad sandbox exceptions. [U4]

### 3.2 Diagnose the entire request, then fix the default

Create a read-only `templar doctor portals` section, or extend the existing equivalent, that records: session type; actual compositor socket; portal frontend and backend versions; desktop environment; selected configuration file including higher-precedence user overrides; advertised interfaces; D-Bus activation health; and request completion/error. Redact filenames and credentials by default.

The source-confirmed defect is:

- `file_chooser.rs::NativeUi::choose` calls `spawn_blocking`.
- `dialog.rs::run` calls `pixelkit_windowing::run_app` for each request.
- PixelKit builds a new default winit event loop.
- `.await.ok().flatten()` turns a failed task join into a normal-looking empty result.

Fix the error reporting immediately: cancellation and infrastructure failure must not have identical diagnostics. The installed browser may instead be using a toolkit-native picker or a different portal; record its actual route rather than declaring the worker-thread defect the confirmed runtime cause. [E03–E05; U1]

Package `xdg-desktop-portal-lxqt` and its target-release dependencies into the same declared seed/closure process as PCManFM-Qt. Resolve them against the **archive's configured Ubuntu snapshot**, not an arbitrary current package repository. The submitted archive already includes GTK portal and Qt Wayland support; their presence is not proof that every selected backend is installed or effective. [E24]

Use a desktop-specific vendor configuration under `/usr/share/xdg-desktop-portal/templaros-portals.conf`. A minimal routing target is:

```ini
[preferred]
default=gtk
org.freedesktop.impl.portal.FileChooser=lxqt;gtk
org.freedesktop.impl.portal.Settings=templaros;gtk
```

This is a **proposed base**, not a complete drop-in file. Generate/validate the final per-interface table against installed `.portal` descriptors. An ordered list is backend-selection preference, not automatic recovery from a selected backend crashing. Keep the identity `XDG_CURRENT_DESKTOP=TemplarOS`; do not impersonate LXQt just to influence selection. Preserve real user overrides and migrate only configuration proven to have been generated by Templar. [U5]

AppChooser can temporarily use an installed GTK build that advertises it. Background needs an explicit decision: GTK's upstream portal descriptor does not advertise Background. Do **not** route it to GTK and assume it works. Until the Templar shared dialog host is repaired, make that capability honestly unavailable or retain only a tested noninteractive policy path. Never silently approve a permission/background request as a fallback. Re-enable Templar Background after its interactive path passes the same host/cancel tests. [U12]

Similarly, do not retain `ScreenCast=cosmic` and `Screenshot=cosmic` merely because those strings are in the current file. Verify both the packaged backend and the compositor protocols it needs. File picking should ship independently of later screen-sharing work. Document unsupported capture rather than select an unrelated backend and leave applications hanging.

### 3.3 Activation, parenting and repeated use

Session startup already imports key environment values into the systemd user manager. Preserve that work; verify it for **all selected portal services**, not just the Templar service. Publish the actual ready Wayland socket, not an unverified constant, before launching graphical clients. Audit direct D-Bus activation where it differs from systemd service activation. Avoid importing the entire environment or exposing secrets. A stale portal process from another desktop/session must not keep old display state. Do not create a second user bus inside the ordinary login session. [E19; U6]

Preserve the current greetd/PAM-session treatment of authorization helpers. Repackaging a polkit agent into an arbitrary user unit can change its session association; native settings must continue to produce visible authorization prompts.

Add or verify `xdg-foreign` parent-handle support and activation behavior. Foreign-toplevel-list is not a substitute. A dialog must belong to its originating browser, remain reachable above fullscreen content when appropriate, and stay on a usable output after a topology change. Empty/expired parent handles need a visible, conservative fallback rather than a dead request. [U7]

The native portal repair should use one persistent main-thread event loop, with D-Bus work sending requests through channels/EventLoopProxy. The process may own multiple dialog windows but must not create a new event loop for every request. A bounded per-request helper process with its event loop on its own main thread is an alternative if the existing host is substantially easier to reuse that way; choose one design, not both. Prefer the persistent loop for the planned Templar backend. Cancellation must close a real window, remove the request and emit one terminal response. A vanished caller or failed display connection is not an indefinite wait.

Do not turn on `GTK_USE_PORTAL=1` globally. Prefer normal desktop integration; any browser-specific setting must be justified against the browser build actually shipped and tested. LXQt's own documentation warns against global use. [U2]

### 3.4 Complete the other file routes

Keep these distinct:

| Action | Integration path |
|---|---|
| Attach/Open/Save | FileChooser portal, or a tested native toolkit path for supported apps. |
| Open a file/URL in its default app | Existing Associations1 plus interoperable MIME defaults/public OpenURI behavior. |
| Reveal downloaded file | Test `org.freedesktop.FileManager1.ShowItems` and the selected file manager's supported behavior; implement a narrow adapter only where required. |
| Open folder | FileManager provider and valid `inode/directory` association. |
| Add folder to Homegarden / make project | Existing PCManFM custom actions into `templar home place` / project APIs. |

Do not implement `org.freedesktop.impl.portal.OpenURI`: that instruction is stale in the current work ledger, and the current backend source already explains the correct distinction. The public OpenURI frontend and AppChooser/default-association integration are the real targets. [E02, E18, E21; U8]

Keep PCManFM in file-manager mode, never `--desktop`; Homegarden owns the desktop. Preserve the recent app-discovery fix that excludes LXQt-only desktop-manager entries from ordinary application identity. Verify one FileManager1 bus owner and one selected browser/folder association. Test the actual desktop-file IDs rather than substituting the Templar canonical AppId into `mimeapps.list`.

### 3.5 Avoid damaging bookmarks or inventing integration

The present filed export rewrites the complete GTK bookmarks file using only its own database, and the implementation uses a direct write. That can discard bookmarks added through another application. Its “one-way authority” comment does not prevent unmodified PCManFM from being another writer. [E14]

For the first delivery, stop automatic full replacement of an existing unmanaged bookmarks file. Preserve it and import existing entries only under an explicit migration policy. Make exporting Home places additive and user-triggered, with a backup, duplicate handling and an atomic write. Retain a sidecar record of entries Templar exported; never remove an entry it cannot prove it owns. If the file changed since it was read, do not overwrite the new version—report a conflict/retry from a fresh read. Concurrent uncooperative writers remain an integration limitation; do not call that a solved multiwriter store.

A later adapter can serialize PCManFM bookmark edits through filed if justified. That is not a prerequisite for browser uploads. Likewise, native PCManFM copy/move/trash operations do not automatically acquire filed's journal/undo guarantees. Use existing filesystem watching to update the shared index, and reserve “managed operation” UI for operations actually submitted to filed. Do not infer a file was opened merely from a file-stat event. Cross-application recents and Homegarden evidence need explicit, trustworthy events.

## 4. Integrate the display utility at the right layer

### 4.1 Reuse the alpha selectively

Extract and review its output model, layout intent, profile matching, preview state machine ideas and test cases. Keep the standalone XRandR adapter for Lubuntu X11. Replace the eframe/egui interface with a PixelKit Displays page for Templar, and use the same reusable pure model where practical.

Do **not** start the alpha's independent recovery helper in TemplarOS. Its daemon, private persistence and wlr-randr polling would otherwise become a second display-policy authority. On Templar, the compositor owns modesetting and the recovery deadline; sessiond stores user preferences and exposes a typed client interface. Closing or crashing Settings must not disable recovery. The alpha is still uncompiled/unvalidated source in this review, not an integration-ready library.

### 4.2 Fix hybrid GPU output ownership first

`add_device` currently returns immediately for a non-primary GPU. On the recorded hybrid reference topology, the internal panel and HDMI can belong to different GPUs. Removing the guard alone is not a multi-GPU implementation. [E06]

Introduce a per-device output backend with explicit render-device versus scanout-device identity. Validate buffer allocation/import, supported formats/modifiers, synchronization, output lifetimes and resume/reprobe. Use the repository-pinned Smithay integration as the reference, not a blind library upgrade. A measured copy fallback is preferable to an unsupported cross-device import, but it must have a documented performance/power cost and bounded failure behavior. Failure of one GPU/output should leave another healthy output running.

Preserve current completion tracking, conditional synchronization and stalled-repaint recovery. Do not reapply an old audit as if the newer render code were unchanged. Integrate lock-coverage tracking with actual presentation completion, not a synthetic “frame submitted” call.

### 4.3 Output inventory and transaction model

Connected, enabled, presenting, usable, lid-hidden and unsupported are separate states. Advertise all supported modes, not just the initially selected one. Use stable identity from device topology and EDID information where available, handle absent/duplicate serials, and retain the connector as a discriminator. Never use the current hardcoded serial `1` or a temporary `cardN` number as persistent monitor identity. [E07]

Expose a topology generation and supported features per output. Proposed operations: inspect, validate, begin preview, confirm, revert, and restore known-good. The transport can be a sessiond API feeding a bounded compositor command channel, using the project's versioned integration conventions. The compositor must own the complete transaction; a dead sessiond/UI cannot stop its timeout.

Validation and application rules:

- Reject stale generations and conflicting previews. Do not mutate live state while validating.
- Test modes/CRTC assignment and resource limits through the real backend.
- Preserve a reachable output for an interactive session whenever usable hardware exists. A genuinely headless system is a valid state; do not invent an output or force a closed/damaged panel.
- Enable and establish the destination before disabling the previous screen where hardware permits. Treat multi-GPU changes as a coordinated operation with partial-failure recovery, not an assertion of cross-device atomicity.
- Start the human confirmation deadline only after the preview is meaningfully presented. Use a short operational timeout while waiting for hardware, separately from the nominal 15-second confirmation window.
- Revert on timeout, failed application, caller loss where appropriate, or invalidated topology. Re-enumerate before recovery; an unplugged output cannot be restored.
- Persist only confirmed profiles. Keep the last confirmed arrangement independently of an unfinished preview journal.

KMS is the layer that validates hardware modes, buffers and exposed properties. Application geometry changes are not a substitute for configuring a connector. [U9]

Provide a compositor compatibility adapter for wlr-output-management only after the native transaction engine works and its authorization model is defined. `wl_output`/xdg-output advertisement does not establish that protocol. Never run `xrandr` under XWayland and report that the physical Wayland desktop was configured.

### 4.4 The visible Displays page

Show connected-but-disabled TVs immediately. Primary choices are **Extend, Duplicate, Show only on 1, Show only on 2**. Name the devices as well as numbering them; numbers are a UI label, not saved identity. Support three-plus outputs through per-output selection without redesigning the two-screen fast path.

Provide Identify, arrangement dragging with keyboard/numeric alternatives, main-output selection for Templar shell placement, resolution, exact refresh choices, orientation, and separate UI scale. Preserve mode distinctions such as 59.94 versus 60 Hz. Do not call fractional UI scaling “resolution.” Duplicate must be real compositor mirroring with an explicit logical scene and scaling policy, not two independent outputs coincidentally placed at the same coordinates.

A new TV should normally extend while preserving the existing usable screen. A confirmed TV-only profile applies to that connected set, not to the laptop alone forever. On HDMI removal, recover another usable screen and move dialogs/cursor/essential shell UI into its logical work area. On reconnect, restore the saved arrangement only after it validates. Handle rapid plug/unplug, delayed EDID/mode availability, docks, lid state, resume, and greeter-versus-user-session profiles.

The historical HDMI note records a failed laptop backlight and a TV mode that worked. Treat it as historical hardware evidence, not current certainty. Allow a user-declared “do not use as automatic fallback” screen; store a known-good TV mode. A program cannot guarantee a physically broken panel will become visible. Login/recovery needs a conservative machine-level output fallback, separate from a user's possibly unavailable preferences.

### 4.5 TV edge fitting without fake resolutions

Expose **Fit picture to TV** separately from signal resolution and UI scale. Use a full-screen calibration target on the selected output, visible corner handles, numeric/keyboard controls, an aspect lock, reset and normal preview/revert behavior. Start with symmetric borders; independent edges can follow after mapping tests.

Prefer real underscan/border properties when advertised and validated. The DRM documentation shows such properties are driver-specific, so absence must be represented honestly. [U9]

For Templar Wayland, the durable fallback is a compositor-owned final output inset: render the same desktop into an inner rectangle, fill the remainder black, and retain the chosen HDMI timing. For example, 48-pixel horizontal and 27-pixel vertical margins inside a 1920×1080 signal leave an 1824×1026 picture. This is a proposed rendering transform, not proof that the current backend already supports it.

The transform must apply consistently to cursor placement, absolute-input mapping, hit testing, damage and screenshots/capture policy. Prevent direct scanout/overlay paths from bypassing it unless they can implement it equivalently. Locked content must cover the entire physical output; black borders cannot reveal an older framebuffer. The rendering cost must be measured before promising acceleration. NVIDIA's X11-specific control path is not a Wayland fallback; retain it only as separately tested standalone-X11 work if added later.

## 5. One obvious settings route

Keep `templar-settings`; do not create a rival control center. Make page identity a stable slug rather than a numeric index. Support direct routes such as `--page network`, `--page sound`, `--page displays`, `--page input`, and preserve old page names as aliases.

Recommended navigation groups:

| Group | Pages |
|---|---|
| Connections | Network; Bluetooth. |
| Devices | Sound; Displays; Mouse and Touchpad; Keyboard. |
| Personal | Appearance; Homegarden and Rails; Accessibility. |
| Applications | Default Applications; Background Activity; Permissions; Providers under Advanced. |
| System | Power and Battery; Updates and Recovery; Storage; About. |

Existing Home Field, association, background, history, hardware, resource and provider functions must remain reachable during migration. Put low-level diagnostics under an Advanced/Diagnostics route rather than promoting them as Homegarden content. Settings search can index page/control labels; it must not require a new semantic-search service.

The network icon, machine rail, sound shortcut, battery item and display shortcut should deep-link to the same page or the same shared quick panel. “One route” means one canonical behavior and owner, not a prohibition on useful shortcuts. Route through Providers1 and existing typed actions, preserving the working `network-status-ui`/nm-tray-2 integration until a native replacement passes its tests. An absent provider needs a visible explanation and a supported installation/recovery path, not an invisible process launch.

### Network

Keep NetworkManager as connection authority. Deliver network status, scan, connect, disconnect, forget, autoconnect, hidden network, password prompt, Ethernet and USB tethering status. Represent unavailable hardware, missing firmware, rfkill, incorrect password, missing agent and authorization denial separately. Ordinary “connected” should not falsely mean Internet access; expose captive-portal/connectivity state where the backend supports it.

Finish the secret-agent path before replacing the existing network UI. Agent-owned credentials belong in a supported secret store, not the generic Settings database, shell arguments, or logs. Request/cancel handling matters as much as displaying a password field. Keep enterprise/VPN details delegated to a tested provider until implemented rather than exposing partial editors that silently discard settings. [U10]

### Bluetooth

Use BlueZ discovery/device state and a proper pairing agent. Show power/rfkill, discovery, pair, connect, disconnect and forget; handle passkey entry/confirmation, rejection and cancellation. Cancel scanning when the relevant UI is gone unless explicitly requested. Trust is an explicit choice, not a shortcut to “pair everything.” Audio routing after pairing belongs to the Sound page's same backend. [U11]

### Sound

Extend existing Audio1 with output and input inventories, availability, default selection, volume, mute, test sound and microphone level/test where implemented. Use stable device attributes to restore preferences, not ephemeral node IDs. Add HDMI, USB and Bluetooth appearance/disappearance behavior and an intelligible fallback when the selected device is lost.

The current wpctl adapter is a deliberate isolation decision, not automatically technical debt. Keep it for simple operations if suitable, adding operation timeouts, bounded output and validated arguments. For a complete inventory/event stream, choose and test a supported PipeWire/WirePlumber integration; do not invent a PipeWire D-Bus API or parse localized decorative output as a stable data format. An isolated bridge is acceptable if measured need justifies it; it is not a reason to create an audio server. External hardware/application changes must refresh the UI. [E09]

### Mouse, touchpad and keyboard

Extend the existing compositor application of tap-to-click. Add pointer speed, supported acceleration profiles, primary button, natural scrolling, supported scrolling modes, tap-and-drag and disable-while-typing. Use actual per-device capability discovery; preserve sane keyboard/pointer access after configuration changes. Include keyboard layout and repeat controls through the proper existing input owner. Apply defaults to newly attached devices and keep stable per-device overrides without using volatile event-node numbers.

Offer Reset and a keyboard-accessible reversal route. Display scaling, TV fitting and input transforms must share the same coordinate definitions. Do not add global high-frequency polling to make controls look live.

### Power, battery and lock

Preserve Battery1, Backlight1, screen-off behavior, manual lock UI and its PAM authentication path. Update stale comments that say manual lock is missing. Implement the remaining stored idle/lid policy rather than drawing switches around unexecuted values. [E10]

First fix lock acknowledgment: record the pending lock confirmation, blank/cover every enabled output, and acknowledge only after the corresponding safe frames are presented. Include new outputs and scanout changes while locked, and keep the session locked if its UI dies. The protocol explicitly requires the presentation barrier; an early success acknowledgment is not merely a cosmetic discrepancy. [E11; U13]

Use logind's power actions and inhibitors. Before taking over lid/power-key behavior, establish a single handler and a fallback when Templar is absent. For lock-before-suspend, acquire the appropriate delay inhibitor before relying on PrepareForSleep, wait for confirmed coverage, and honor the bounded delay deadline. Test resume with HDMI removed or attached. Only enable hibernation where the installation's resume path is actually supported. [U14]

The docked behavior must distinguish “an external connector exists” from “an external display is currently usable.” A black, disconnected or ignored HDMI connector must not make closing the lid strand the session.

## 6. PixelKit consistency is functional work, not just colors

PixelKit already supplies Theme, operational palettes, widgets, HiDPI conversion, multi-window hosting and optional accessibility hooks. Extend it instead of making another widget library. Keep Templar-specific design tokens/behavior adapters in a small shared crate; keep hardware policy and D-Bus out of generic PixelKit. The compositor may consume pure tokens/raster/text, not the transitive winit host through pixelkit-ui. [E13, E25]

Define a documented type scale, spacing rhythm, icon bounds, focus ring, selected/hovered/disabled states, destructive actions, inline validation, empty/loading/error states and dialog button conventions. A practical initial proposal is a 4-unit spacing base, 14-unit body text, 12-unit supporting text and 20-unit section titles, all in logical units with scalable controls; these are starting design values, not measured accessibility guarantees. Validate them against the actual display density and fonts before freezing them.

Make shared patterns for settings rows, password entry, device pickers, progress, confirmation and actionable error banners. Apply them first to Settings and machine controls, then launcher/switcher/notifications, greeter and lock screens, and finally the experimental native portal. Avoid restyling every legacy page before essential workflows work.

Essential toolkit deliverables:

- **Real Linux clipboard:** Wayland copy/paste integrated with the live window/seat and asynchronous ownership/lifetime, plus X11 support for standalone Lubuntu use. A shell clipboard manager does not make PixelKit's current local fallback a system clipboard. Keep test fakes explicit and report unavailable clipboard rather than successful local-only copy in production.
- **Text and IME:** correct composition/commit, selection, cursor movement, caret placement and pasted Unicode; test Latin, Thai and non-Latin filenames. Do not log passwords or copy them into history.
- **Keyboard and accessibility:** stable focus order, visible focus, Escape/cancel, expected Enter behavior, meaningful labels/roles/checked states. PixelKit's AccessKit hooks require applications/widgets to populate a tree; enabling a Cargo feature alone is not completion.
- **Scaling:** logical layout and physical raster coordinates converted exactly once; test 100%, 125%, 150% and 200%, mixed displays and resizing. Add regression cases for the popovers and dialogs, not only a demo widget grid.
- **Motion:** propagate the existing reduced-motion preference to new popovers, display transitions and October 4 park/restore effects. Do not break frozen Alt-Tab ordering or stable Home-card locations to make transitions decorative.

For GTK and Qt, align the supported font, light/dark preference, cursor/icon theme and available accent facilities using packaged, reversible configuration. Verify the needed Qt5/Qt6 platform plugins for applications actually shipped. Do not promise pixel-identical third-party applications or force unsupported CSS overrides. Keep Templar's portal Settings values synchronized with its appearance settings and publish changed signals. Toolkit differences in the temporary LXQt chooser are acceptable if it remains readable, familiar and dependable.

## 7. Work and application integration without undermining Homegarden

Use the October 4 active-work direction: keep folder-clustered work-linked cards in Homegarden, including their stable locations when windows are parked. Do not revive the older “remove every running window from Home” proposal and undo the newer design. Reconcile the old purity record explicitly. Homegarden is still not a performance dashboard. Taskfield remains the overview of running applications/workloads; detailed CPU/GPU/memory diagnosis belongs in the diagnostic tool. [E17]

One WindowId/AppId/InstanceId model should drive Home cards, Taskfield, Island, Alt-Tab, notification attribution and Open With. An application's installed variant is its identity; grouping variants visually must not merge their permissions or resources. A launcher's requested identity, a window's claimed app_id and a verified process/cgroup identity are not interchangeable.

Complete external-launch integration, including launches made by PCManFM, browser helpers and the public portal frontend. Maintain the existing association export, regenerate/validate it when catalog state changes, and treat conflicting user defaults with an explicit migration policy. Do not claim every external GAppInfo launch enters Launcher1 automatically. Initially support the shipped launch paths with narrow adapters or trustworthy process/cgroup adoption; unknown processes must remain unknown rather than be granted privileges based on a title or executable basename. [E21]

Application detail should say what is **enforced**: background permission, controlled resource policy, sandbox grants, update source and supported persistence. A manifest checkbox is not filesystem/network isolation. For an unsandboxed native application, do not present a deny-files or deny-network switch unless a real enforcement backend exists. Keep the current packaging mix; Flatpak migration is not required to repair dialogs or displays.

Adopt the existing opt-in workload proposal: `templar run -- COMMAND` gets a distinct WorkloadId and scope linked to its originating application instance. Preserve terminal job control, exit code, signals and Ctrl-C. Do not install global hooks that reclassify every shell command by its process name. Measure and test a supported command list before optional automatic integration. Distinguish foreground interactive work from background compute; placing every workload into a low-priority background class would be another misleading simplification. Memory-pressure action on a build job should not kill the user's terminal or unrelated editor.

## 8. System ownership and recovery: make the existing proposal enforceable

This is a separate lane, not a prerequisite for the first working file dialog. However, the existing candidate-path bug is a correctness priority before relying on update recovery.

### 8.1 Declare owners before changing mounts

Extend `distro/deploy/layout.toml` and `components.toml` with checked owners, `system|state|work` tiers, writable paths and explicit exceptions. Use the existing ownership-tiers proposal; wire its validator into the normal gate. The validator should catch package installation into user Work paths, contradictory unit ReadWritePaths/StateDirectory, missing owners and undeclared live-root update paths.

This is a declaration and review aid, not proof of runtime confinement. Complement it with image boot tests and write-denial tests for the promised read-only root. Keep caches, persistent machine identity, application profiles, disposable state and irreplaceable work distinguishable.

### 8.2 Fix deployment identity end to end

The current real configuration stages under `state_dir/deployments`, then the update engine records `candidate.display()` as `Deployment.subvolume`. Current GRUB writing expects a safe Btrfs-relative identifier starting with `@`. Preserve that validation; fix the value passed to it. Do not weaken the validator to accept arbitrary host paths. [E15]

Model these separately: deployment ID; Btrfs-relative subvolume path; mounted candidate path; ESP-relative boot-artifact directory; kernel/module/package set; known-good status. Derive the mounted location and boot identity from the declared layout. Retain the newer UUID-aware root selection and known-good deployment's own subvolume. Existing tests with hand-constructed `@deployments/id` records are not enough: run the **real-config -> stage -> record -> boot-entry** path against a disposable image. [E16, E22]

### 8.3 Close the open ownership decisions

Recommended decisions, using the discretion requested:

**Ordinary native package installation stays allowed**, but becomes a hostd candidate transaction instead of untracked live-root apt work. Existing application formats remain supported; do not force Flatpak conversion.

**Keep dpkg/APT's package metadata with the deployment root.** Split persistent `/var` content into declared state mounts/binds rather than binding all of @var over versioned package metadata. For the initial design, keep the package-manager state together rather than optimizing shared download caches prematurely. A rollback must show matching installed files and package database.

**Use Btrfs snapshot deployments**, consistent with the existing layout and hostd work, rather than introduce a second A/B update architecture. Seal only after package installation, verification and boot-artifact preparation have succeeded. Audit boot-time writers before switching to read-only boot.

**Workload separation begins opt-in**, as described above. Automatic shell hooks are optional later work.

Before read-only rollout, classify runtime `/etc` writers, user/account data, machine identity, network connection secrets, hostd state, journals and application profiles. Do not bind all `/etc` to mutable state or roll back all credentials/accounts blindly. Keep per-deployment defaults separate from narrowly defined persistent host settings. Test first boot, sysusers/account changes, NetworkManager updates and recovery from a failed migration.

Keep hostd's registry/transaction journal outside the rolled-back root and make state schema upgrades backward-aware: stage code may be rolled back while shared state persists. Unknown newer state must cause a safe refusal/read-only mode, not destructive downgrade. Copy/backup before irreversible state migration. Never call root rollback a backup of user Work.

Make `templar dev deploy` use the same candidate path where appropriate, or keep its temporary exception explicit and visible in the checker until converted. Preserve the declared Canonical-signed kernel/NVIDIA pairing and supported boot-manager paths. Do not solve an integration bug by installing a new kernel or changing the base distro.

### 8.4 User-facing recovery

Settings should show current version, candidate version, whether a restart is needed, and the known-good fallback. “Update ready” requires a bootable candidate, not merely downloaded packages. “Healthy” needs the existing boot-health chain plus a usable session/output path; do not promote a deployment solely because processes exist.

Return-to-previous-system must preserve documents and profiles, clearly state what changes, and work from the boot path when the graphical session fails. Retain the known-working Lubuntu installation and existing utilities during validation. The project milestone's eventual removal of Lubuntu is an acceptance outcome, not a setup prerequisite.

## 9. Packaging, testing and deployment discipline

Build all selected routes into the actual image/package closure: file manager, file picker, frontend portal, Qt Wayland support, network firmware/driver selection, authorization helper, secret store integration, UDisks helper, audio components and declared defaults. Do not leave the successful developer-machine `apt install` as the implementation.

Pin/export sibling source dependencies used by the build. The attached PixelKit archive solves only part of the local path closure; the Templar workspace also names an external still265/bpg-rs path, and the agent archives omit fonts. Restore those from their owning source/package with checksums and license notices before claiming reproducibility. Do not lower the workspace toolchain to the old display alpha's MSRV. [E20]

Use four evidence levels: pure-model tests, private-bus/nested integration, installed-image tests, and physical-hardware journeys. Only the last two justify default desktop behavior on the target installation. A headless test of a file-list model does not prove Firefox can open a visible modal or read selected bytes.

Tests must run under isolated runtime/config/home and a private test bus where appropriate. Never restart, mask, or replace the user's live compositor/session/portal services as a side effect of a test. Account for nested systemd user-manager sharing; a private D-Bus address alone does not make calls to the real user manager safe. Use a disposable user/VM for activation tests that need systemd units.

Require nonzero matched test counts. Existing proposed work already identifies the false-green case where a mistyped cargo test filter runs zero tests and exits successfully. Keep evidence tied to the source revision and exact command, with executed/passed/failed/skipped/blocked distinguished.

### Release journeys

| Journey | Pass condition |
|---|---|
| Browser upload | Firefox and Chromium on every advertised packaging/session combination show a chooser, return selected bytes, cancel correctly, and work on the second and third request. |
| Save/reveal/open | Save As, overwrite handling, file filters, multiple selection, folder choice where supported, downloads reveal, default application and Open With all work through the intended route. |
| File state | External file operations update Home/search eventually; no lost native bookmarks; unsupported shared undo/recents claims are absent. |
| HDMI recovery | Show only on TV, confirm, close Settings, unplug HDMI; another usable output and all essential controls recover without a command. |
| HDMI failure | Invalid mode, crash during preview, hotplug storm, mixed GPU, lid close, resume and TV-fit changes preserve a recovery route. |
| Ordinary settings | Connect to a new secured network, pair a device, select HDMI sound, adjust touchpad behavior; the observed system state matches the UI and survives relogin where saved. |
| Secure power | Lock, confirmed coverage on all outputs, suspend, topology change, resume; no unlocked frame, no input leak, safe behavior if the lock client dies. |
| Toolkit | Copy/paste across browser and PixelKit, keyboard-only operation, IME, accessible labels and mixed scaling work in production apps. |
| Deployment | A candidate generated by the real configuration boots; failed candidate recovers; dpkg metadata matches the booted root; Work is unchanged. |

A useful non-destructive browser fixture can verify selected file lengths and hashes locally, without uploading private files to a network server. Attach logs/screenshots only with sensitive data removed. Proposed timing thresholds should be measured on the reference machine before turning them into release promises.

## 10. Order of delivery and current-planning fit

**First:** the portal/file-manager integration slice and an installed browser journey. In parallel, fix error visibility and establish the shared settings/UI contracts. Do not wait for Homegarden animation polish.

**Second:** compositor output inventory, hybrid-GPU support and the safe transaction engine; fix the lock presentation barrier in the same output-completion area. Then connect the PixelKit Displays page and the standalone Lubuntu adapter. Native network/sound/Bluetooth/input pages can progress independently once shared interfaces are settled.

**Third:** finish power enforcement, cross-application identity/defaults/file routes, and visual consistency across the remaining owned surfaces. Stabilize reduced motion and Home/Taskfield behavior rather than adding a new navigation model.

**System lane:** declare ownership and fix real deployment paths early; complete read-only state separation and the rollback image journey separately. Do not hold uploads hostage to this larger migration.

Use the existing AKR work as the parents: daily-driver X1, PCManFM integration, home-field-settings-gui, October 4 polish/test backlog, and the four ownership-tier children. Correct stale intent and evidence before adding duplicate work. New slice labels in BACKLOG are planning labels only—not imported AKR records. Update authoritative records with the project's tools, then regenerate `docs/generated`; do not hand-edit generated status to mark work complete.

The target result is ordinary: attach a file, connect to Wi-Fi, select TV audio, use the TV, unplug it, resume safely, return to the same work, and recover from a bad system update. That is the right test of whether the architecture has become a usable desktop.

## Upstream references

These were consulted for API constraints, not as evidence that Templar's implementation works. Package features must still be checked against the selected build.

- **U1 — winit EventLoopBuilder:** https://docs.rs/winit/0.30.13/winit/event_loop/struct.EventLoopBuilder.html
- **U2 — LXQt portal:** https://github.com/lxqt/xdg-desktop-portal-lxqt
- **U3 — PCManFM-Qt:** https://github.com/lxqt/pcmanfm-qt
- **U4 — FileChooser API:** https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.FileChooser.html
- **U5 — portal configuration:** https://flatpak.github.io/xdg-desktop-portal/docs/portals.conf.html
- **U6 — activation environment:** https://flatpak.github.io/xdg-desktop-portal/docs/system-integration.html
- **U7 — parent window identifiers:** https://flatpak.github.io/xdg-desktop-portal/docs/window-identifiers.html
- **U8 — public OpenURI API:** https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.OpenURI.html
- **U9 — Linux DRM/KMS:** https://docs.kernel.org/gpu/drm-kms.html
- **U10 — NetworkManager secret agent:** https://networkmanager.dev/docs/api/latest/gdbus-org.freedesktop.NetworkManager.SecretAgent.html
- **U11 — BlueZ pairing agent:** https://bluez.readthedocs.io/en/latest/agent-api/
- **U12 — GTK backend descriptor:** https://github.com/flatpak/xdg-desktop-portal-gtk/blob/main/data/gtk.portal.in (inspected blob d9b568eabdbf760c22adb61218995b6cdf3ee476).
- **U13 — session-lock protocol text, rendered from upstream XML:** https://wayland.app/protocols/ext-session-lock-v1 (normative text only; not the site's compositor-support table).
- **U14 — systemd inhibitors:** https://systemd.io/INHIBITOR_LOCKS/
