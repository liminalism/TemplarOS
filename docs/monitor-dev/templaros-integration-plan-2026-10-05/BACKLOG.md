# Implementation backlog and acceptance gates
## TemplarOS daily-driver integration — October 5, 2026

Use with [PLAN.md](PLAN.md) and [EVIDENCE.md](EVIDENCE.md). These are proposed implementation slices, not commits, imported AKR records, or executed tests. Source paths are relative to the TemplarOS archive unless prefixed `PixelKit/`. Existing `@legeos.*` record keys are deliberately preserved; a branding cleanup must not silently change persistent record identity.

## 1. Attach to the current planning, not a competing roadmap

| Existing record | What to change or add |
|---|---|
| `@legeos.work.developer-edition-daily-driver/1` | Make the first outcome a successful browser upload/save journey using a supported external chooser. Correct the nonexistent `impl.portal.OpenURI` instruction. Native chooser completeness is a separate later child. Refresh the capability matrix against current code. |
| `@legeos.work.pcmanfm-qt-integration/1` | Keep existing provider, package and custom-action implementation. Add browser/file-manager separation, reveal/defaults tests, and non-destructive bookmark interoperability. Close its pending hands-on check only with installed evidence. |
| `@legeos.work.home-field-settings-gui/1` | Keep it as the owner of display/input/appearance controls. Add the display engine and toolkit tasks below, not a second monitor-settings project inside Templar. |
| `@legeos.work.desktop-polish-and-test-infrastructure-backlog-20261004/1` | Put shared tokens, dialog behavior, accessibility, real clipboard, scale tests, route consistency and test isolation here. Do not begin with cosmetic animation work. |
| `@legeos.work.bar-cluster-clickable-20260908/1` | Preserve role-based network behavior while adding canonical settings deep links. Replace a provider only after its successor actually connects/pairs/selects devices. |
| `@legeos.work.home-work-centred-folder-clusters-20261004/2` | Preserve the newly requested work-card clusters and stable parked-card locations. Add file-route/identity regression cases. |
| `@legeos.work.alt-tab-and-task-rail-icons-20261004/1` | Preserve shared application/window identity, parked windows and frozen switch order during the settings/polish changes. |
| `@legeos.work.home-purity-20260908/1` | Reconcile its older removal language with the newer work-linked-card direction. Keep the prohibition on generic diagnostic clutter, not a prohibition on all active-work cards. |
| `@legeos.work.ownership-tiers-20261002/1` | Adopt the recommended decisions in PLAN §8 and retain its existing children. |
| `@legeos.work.ownership-tier-declaration-20261002/1` | Implement declared owners/write paths and the checker first. |
| `@legeos.work.hostd-deployments-follow-the-layout-20261002/1` | Refresh the stale observation against E15/E22: some boot-entry correctness has improved, but real config/serialized subvolume remains inconsistent. |
| `@legeos.work.read-only-system-deployment-20261002/1` | Pair package metadata with root, enumerate runtime writes, and test rollback without changing Work. |
| `@legeos.work.terminal-workload-identity-20261002/1` | Implement opt-in workload scopes; test signals and job control before optional hooks. |
| `@legeos.work.acceptance-checks-that-cannot-fail/1` | Require an observed nonzero test count; no green gate based on an empty filter match. |

Update authoritative records first and regenerate `docs/generated`. Do not copy these proposed labels into status snapshots and report them as accepted work.

## 2. Slice contracts

### S00 — Freeze the source baseline and make test results truthful

**Owner:** build/test integration. **Dependency:** none. **Existing parent:** daily-driver and test-infrastructure work.

Record archive/source revisions, toolchain, selected package snapshot and sibling dependency versions. Restore excluded fonts and required external source inputs from their owning packages/checkouts. Add isolated HOME/XDG/runtime/bus fixtures, explicit checks against contacting the live user service manager, and nonzero-test-count verification. Baseline current portal, settings, Home-card, Alt-Tab and boot-entry tests without editing behavior.

**Locations:** root Cargo/toolchain files, `distro/base.toml`, package closure tooling, existing nested-session harnesses, checked interface tooling.

**Gate:** a deliberately misspelled test filter fails the gate; a nested run cannot restart/mask the active user's session; source closure omissions are reported, not hidden.

### S01 — Restore browser file dialogs with the supported picker stack

**Owner:** desktop/portal integration. **Dependency:** S00 testing isolation, not completion of all other baseline tests.

Add LXQt portal through the seed and package closure. Ship/validate the desktop-specific routing table, preserve user overrides, and verify activation with the actual ready compositor socket. Route FileChooser to LXQt; keep GTK as a selection alternative. Audit AppChooser/Background separately; do not assign unsupported interfaces to GTK. Remove stale implicit assumptions about Cosmic capture.

**Locations:** `distro/seeds/selection.toml`, `portals/xdg-desktop-portal-templar/templaros-portals.conf`, `session/templar-session/src/main.rs`, portal/user units and packaging rules, existing doctor code.

**Gate:** fresh installed session; Firefox and Chromium; attach a known local file and verify bytes; cancel; repeat at least three requests; Save As; backend missing/failing gives a diagnosable result. Cover every browser packaging format the release advertises. Do not send personal files to a remote test endpoint.

### S02 — Repair the native portal host and its failure semantics

**Owner:** PixelKit/portal integration. **Dependency:** S00; independent of shipping S01.

Run one graphical event loop on the process main thread. Forward asynchronous requests into it, support more than one dialog window, and retire per-request `spawn_blocking -> run_app`. Distinguish user cancel, display failure, worker failure and request timeout. Preserve parent handles and activate/position conservatively. Repair FileChooser, AppChooser and Background together because they share the host.

**Locations:** `portals/xdg-desktop-portal-templar/src/{main,dialog,file_chooser,app_chooser,background}.rs`; PixelKit host API only where genuinely needed; compositor parent/activation protocols.

**Gate:** repeated/concurrent requests, caller disappearance, cancel, backend restart, missing display, fullscreen parent and moved/unplugged output. Re-enable native interactive Background only after its UI gate. Keep LXQt the default FileChooser while the custom browser journey remains incomplete.

### S03 — Finish PCManFM integration without data loss

**Owner:** file-service/application integration. **Dependency:** S01 for browser journey; other changes can proceed independently.

Verify provider selection, `inode/directory`, actual desktop-file IDs, Open With, default open, downloads reveal and FileManager1 ownership. Preserve the filtering of LXQt desktop-manager entries. Keep DES-EMA Home/project actions. Replace unconditional full bookmarks rewrite with the explicit preservation/additive-export policy in PLAN. Observe native operations through the shared index without claiming filed undo for external copies.

**Locations:** `crates/templar-session-core/src/providers.rs`, `crates/templar-appd/src/desktop_entry.rs`, `crates/templar-associationd/src/{dbus,export}.rs`, `services/templar-filed/src/bookmarks.rs`, CLI custom-action packaging and shell open-in-file-manager route.

**Gate:** no `pcmanfm-qt --desktop` in Templar; old/native bookmarks survive export/relogin; rename/move/trash reflected in search; browser reveal selects the right file; filenames with spaces, Unicode and shell metacharacters are not executed.

### S04 — Shared settings contracts and canonical navigation

**Owner:** session API/settings. **Dependency:** S00; settle interfaces before parallel page implementation.

Add stable page slugs, aliases and search labels. Define common capability/revision/operation/error data. Put ordinary pages first while retaining advanced functions. Deep-link shell controls through typed actions/provider resolution to one destination. Persist only through owned APIs and show applied versus desired state.

**Locations:** `apps/templar-settings/src/{main,surface}.rs`, `crates/templar-types`, `crates/templar-client`, `crates/templar-session-core/src/settings.rs`, `interfaces/dbus/`, shell action routing.

**Gate:** GUI/CLI agree; current advanced pages still work; unsupported row explains why; externally changed state refreshes; incompatible interface changes fail the existing signature checker.

### S05 — PixelKit production interaction and design-token foundation

**Owner:** toolkit/shared UI. **Dependency:** S00; consumes S04 error/capability conventions.

Add real Linux clipboard behavior, event-loop-compatible asynchronous delivery, text/IME and keyboard-focus tests, semantic accessibility adapters, scalable standard controls and Templar-owned declarative tokens. Keep generic toolkit free of OS policy. Do not introduce winit transitively into the compositor core through pixelkit-ui.

**Locations:** `PixelKit/crates/pixelkit-shell/src/clipboard.rs`, `PixelKit/crates/pixelkit-windowing/src/host.rs`, `PixelKit/crates/pixelkit-ui/src/`; proposed pure token/shared Templar UI module.

**Gate:** cross-process copy/paste in a browser and standalone Settings; fake clipboard explicitly test-only; IME and Unicode filenames; screen-reader semantics where supported; 1x/1.25x/1.5x/2x renders and keyboard-only operation. Existing other PixelKit applications retain behavior.

### S06 — Network and Bluetooth as effective controls

**Owner:** device-policy/UI integration. **Dependency:** S04; S05 for new native prompt widgets.

Keep existing providers until replacement succeeds. Add NetworkManager state/scan/connect/forget and cancellation/secret-agent integration; add BlueZ device/pairing-agent behavior. Use existing authorization and secret-store components with correct session ownership. Limit advanced network configuration to fully supported paths or delegate it visibly.

**Locations:** new bounded modules under `services/templar-sessiond/src/apis/` as justified, typed interface/client additions, existing providers and Settings pages, seed/service inventory.

**Gate:** new secured Wi-Fi, wrong password, rfkill, missing firmware, captive network distinction, USB tether; Bluetooth passkey/cancel/reconnect/forget. No secrets in JSON settings or logs, no competing agents silently answering the same request.

### S07 — Real audio-device selection

**Owner:** audio/UI integration. **Dependency:** S04; parallel to graphics.

Extend Audio1 inventory/default selection and external-change behavior. Harden the existing wpctl runner with timeout/output bounds; choose a supported event/inventory adapter after reviewing its isolation requirements. Support input/output selection, hardware disappearance and persisted stable preference.

**Locations:** `services/templar-sessiond/src/apis/audio.rs`, `interfaces/dbus/org.templaros.Audio1.xml`, client, Settings sound page, shell sound controls.

**Gate:** select HDMI sound, Bluetooth and USB headset; remove selected device; mute/volume changed outside Templar appears correctly; failed helper never hangs Settings/sessiond.

### S08 — Compositor output inventory and hybrid-GPU foundation

**Owner:** compositor/backend. **Dependency:** S00; interface design coordinated with S04.

Replace the primary-only scope with explicit supported render/scanout device relationships. Preserve current render/fence/watchdog improvements. Advertise full mode/capability inventory and stable connector identity. Handle per-device failure, remove/readd and resume without abandoning healthy outputs. Do not merely delete the non-primary guard.

**Locations:** `compositor/templar-comp/src/backend/udev.rs`, compositor state/output models, integration transport and GPU test fixtures.

**Gate:** physical reference laptop internal/external topology works, including connectors on separate GPUs; import/copy path verified; unsupported path visibly reported; no whole-session collapse on secondary-device failure. Nested-only evidence is insufficient.

### S09 — Lock coverage barrier

**Owner:** compositor/session security. **Dependency:** current frame-completion machinery; coordinate with S08, do not wait for display UI.

Replace immediate lock confirmation with a per-output presented-safe-frame barrier. Reconcile pending topology changes and lock-client death. Distinguish LockRequested from actual Locked. Keep trusted unlock/PAM and input-isolation behavior; audit lock manager access policy rather than trusting client-supplied labels.

**Locations:** `compositor/templar-comp/src/session_lock.rs`, output completion callbacks, `services/templar-sessiond/src/apis/lifecycle.rs`, shell lock bridge.

**Gate:** no acknowledgment before every active output is covered; no unlocked frame after resume; hotplug while locked stays protected; failed lock client cannot unlock; pointer/keyboard/recovery shortcuts cannot reach hidden session content.

### S10 — Compositor-owned display transactions

**Owner:** compositor and session API. **Dependency:** S08, S04; S09 for lock-aware output changes.

Implement inspect/validate/preview/confirm/revert with topology generations, one active preview, real backend validation, partial-failure recovery and compositor-owned timers. Keep persistence in the owned preference store; unfinished preview is not the last known good. Implement headless and damaged/closed-panel policy deliberately.

**Locations:** new pure display-policy module/crate adapted from alpha `model.rs`/`policy.rs` as useful; compositor output manager; client/API definitions; session settings/profile storage.

**Gate:** timeout, stale request, caller/UI/sessiond crash, disappearing output and partial multi-GPU failure recover. Cross-device atomicity is not claimed. The GUI is not running for the unplug test.

### S11 — PixelKit Displays and standalone Lubuntu adapter

**Owner:** settings/UI and standalone packaging. **Dependency:** S10; S05 controls.

Deliver visible disabled outputs, layout intents, Identify, exact modes/refresh, scaling/orientation, arrangement and accessible control alternatives. Implement true mirroring. Keep one PixelKit page in Templar and a separate standalone entry on X11, sharing pure policy as practical. Do not run the old alpha helper in Templar.

**Locations:** Settings display page, reusable display core, separate XRandR transport/package. Retain wlr-randr only for a compositor demonstrably exposing the required protocol, not as an assumed Templar backend.

**Gate:** TV-only -> confirm -> close Settings -> unplug -> usable fallback; reconnect restores validated profile; keyboard-only arrangement; mixed scale; three-output inventory; X11 regression test remains separate from Wayland.

### S12 — TV picture fitting

**Owner:** compositor/render and display UI. **Dependency:** S10/S11.

Implement advertised hardware underscan first where it works; otherwise implement explicit compositor output inset. Add full-screen calibration target, symmetric corner controls, numeric adjustment, aspect lock/reset and confirmation. Do not change the HDMI mode to simulate safe margins. Keep capture/cursor/input/lock/direct-scanout behavior consistent.

**Locations:** display capability/model, final compositor output transform, pointer/input mapping, full-screen calibration surface and Settings controls.

**Gate:** all corners visible; pointer matches target; full-screen apps cannot bypass fitting; black margins remain safe while locked; cancellation restores image; performance measured with fitting enabled. Independent edge tuning is a follow-on, not required for the symmetric alpha.

### S13 — Input and enforced power policy

**Owner:** compositor input plus session policy. **Dependency:** S04/S05; automatic lock/suspend depends on S09.

Extend existing tap-to-click to supported per-device controls; implement stored idle/lid policy using actual compositor idle and logind integration. Keep one hardware-key/lid handler, correct inhibitors and fallback when the Templar session exits. Wire backlight dimming, battery/mains policies and saved device overrides without claiming unsupported hibernation.

**Locations:** compositor input/idle/state, existing Power1/Backlight1/Battery1 APIs, shell/session lifecycle and Settings pages.

**Gate:** effective settings survive relogin; hotplug defaults; docked and undocked lid behavior; secure lock-before-suspend and resume with changed outputs. No inert control is presented as enabled functionality.

### S14 — Application identity and opt-in workloads

**Owner:** identity/governor integration. **Dependency:** S04 contracts; isolated process/scope tests from S00.

Finish supported external-launch attribution and association export at startup/catalog change. Add WorkloadId and parent relationship using existing proposal; keep terminal foreground/background semantics and explicit launch classification. Present permission enforcement truthfully for native and sandboxed variants.

**Locations:** `crates/templar-appd`, `templar-types` identity/unit parsing, session APIs, CLI, resourced's unit discovery, Home/Taskfield shared identity consumers.

**Gate:** PCManFM/portal launches have trustworthy identity or remain clearly unknown; forged app_id gains no privilege; killing/reclaiming a build scope leaves terminal alive; job control, exit status and signals preserved; no automatic title/process-name identity inference.

### S15 — Visual consistency and Home/work regression completion

**Owner:** shell/settings/UI. **Dependency:** S05; domain pages need not all be complete to begin.

Apply shared type/spacing/focus/menu/dialog/error patterns across owned surfaces. Align supported GTK/Qt fonts/icons/theme state. Propagate reduced motion. Preserve October 4 card clusters, stable parking, full-screen chrome behavior, shared Alt-Tab/Taskfield identity and diagnostic separation.

**Locations:** shared token adapter, Settings and shell surfaces, greeter/lock rendering, distro theme defaults; update current planning conflicts.

**Gate:** screenshots and keyboard journeys in light/dark/mixed scale; no identity/card-position regression; third-party styling uses supported configuration and remains reversible.

### S16 — Checked ownership declarations

**Owner:** system/build. **Dependency:** none beyond S00 tooling safety; parallel to visible UX.

Add tier/owner/writes/exceptions to layout and components. Implement the existing proposed check-ownership gate. Classify runtime state and Work explicitly; record temporary live-root/developer exceptions with decisions.

**Locations:** `distro/deploy/{layout,components}.toml`, unit/package metadata, xtask validation, architecture ownership doc.

**Gate:** injected undeclared Work writes, missing owners, contradictory unit permissions and invalid exceptions fail. The checker does not pretend to be runtime sandbox enforcement.

### S17 — Real candidate-to-boot identity

**Owner:** hostd/deployment. **Dependency:** S16 declaration format; may fix immediate path bug before full checker rollout.

Separate mounted paths from Btrfs/ESP-relative identities. Derive real candidate mount from layout; preserve UUID root identity and strict boot-entry validation. Exercise the real factory path rather than only hand-built fake deployment structs.

**Locations:** `services/templar-hostd/src/{main,updates,boot_entries,deploy}.rs` as applicable, configuration types, installer/provisioner layout handling.

**Gate:** a candidate staged by real config generates a valid entry naming the actual subvolume and boots in a disposable image; known-good entry remains correct; interrupted phase resumes safely. Never weaken validation to accept arbitrary mounted paths.

### S18 — Read-only deployments with compatible persistent state

**Owner:** hostd/installer/system integration. **Dependency:** S16/S17.

Adopt PLAN's explicit ownership decisions. Keep package databases with root, split persistent runtime mounts, classify `/etc` writers, make ordinary installation and developer deployment use hostd transactions, and gate seal/promotion. Design rollback-compatible shared-state schemas and preserve Work/credentials appropriately.

**Locations:** layout/components, installer, image build, hostd transaction and boot-health chain, developer deployment/update scripts, settings recovery page.

**Gate:** read-only runtime boot with no unexplained EROFS failures; update then rollback pairs dpkg with binaries; failed boot recovers; application profiles and documents unchanged; interrupted migration is recoverable. Physical boot/driver evidence required before default promotion.

### S19 — Installed daily-driver acceptance

**Owner:** release/hardware. **Dependency:** select completed slices for an explicit release scope; incomplete optional features labeled unavailable.

Run the matrix in PLAN §9 on a fresh installed image and an upgrade from the previous supported image. Record actual package/runtime versions, source IDs, tests and hardware. Include visible behavior, not just service status. Verify private tests have not changed the user's live session. Keep a known-good boot and the existing fallback desktop until acceptance is signed off.

**Gate:** every advertised operation passes its user journey. A failed mandatory portal, display recovery, secure lock or rollback gate prevents the corresponding default from shipping.

## 3. Parallel work without conflicting ownership

The portal/files lane can ship its first useful result independently. The toolkit/settings lane should settle shared contracts before multiple domain pages modify them. The compositor lane owns GPU, display-transaction and lock-barrier work, with one reviewer for output lifetime changes. The system lane handles layout/hostd; it must not force a package or boot migration into the portal fix.

Coordinate interface/schema changes before splitting work. Do not have multiple workers rewrite `surface.rs`, provider selection, or shared interface XML independently without agreed boundaries. Small commits should include behavior, tests, capability/status updates and rollback instructions together.

## 4. Definition of done for each slice

An implemented slice has a named owner, bounded source changes, compatible interfaces, effective-state reporting, failure/cancel behavior, package/install integration where needed, and recorded test evidence. A default-changing slice additionally has installed-session evidence. Source comments, generated roadmap status, mock tests and screenshots cannot substitute for the browser/hardware journey they purport to verify.

No slice in this document is marked completed by this review.
