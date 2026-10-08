# Source evidence — TemplarOS integration plan

Review: October 5, 2026. This appendix is an inspection record, not a test result. `TemplarOS/` below means the `LegeOS/` root inside the supplied archive; `PixelKit/` means its supplied `pixelkit/` root. Line numbers refer to those unmodified extracted files. Comments may be stale; the plan distinguishes executable behavior from comments and planning intent.

No Rust compilation, VM boot, browser journey, graphics modeset or installed-system diagnostic was performed. The archive omits production/test fonts and some external sibling dependencies. No repository or Library content was changed.

## Input fingerprints

- `LegeOS-agent-pack-2026-10-05_040133.7z` — SHA-256 `cef0ee9b9a00b6ce4d7919afb7d3d577c948d420610355b32dbe9f9586249296`
- `pixelkit-agent-pack-2026-10-05_040202.7z` — SHA-256 `17c108bc323d43a5214bd5233aa2bca39f06d9397fbd0cce9811a0e00bc47662`
- `templar-displays-alpha.zip` — SHA-256 `c777e8a875f1690724a43383fd04f372e71642fe6c186452e1b44372f17d96c0`

<a id="e01"></a>
## E01 — The existing process boundaries already provide a coherent control plane

**`TemplarOS/ARCHITECTURE.md:24–61`**

```text
24: │ templar-boot-health (system scope) → boot-complete.target → systemd-bless-boot     │
25: └────────────────────────────────────────────────────────────────────────┘
26:         Ubuntu 26.04 LTS: systemd, cgroup v2, apt, btrfs, PipeWire, NetworkManager, BlueZ, Mesa, NVIDIA LRM
27: ```
28: 
29: ## Processes (why these and not more)
30: 
31: | Process | Scope | Why separate |
32: |---|---|---|
33: | `templar-comp` | user, session.slice | Wayland display server; a crash disconnects clients → conservative, minimal |
34: | `templar-shell` | user | everything decorative; restartable at will |
35: | `templar-sessiond` | user | **the** policy daemon: identity/launch, associations, providers/autostart, history, notifications, settings, projects, power, location — one state DB, one crash unit, no cross-daemon races (ADR-0001) |
36: | `templar-resourced` | user | must tick while everything else thrashes; no SQLite/fsync/D-Bus on its loop (ADR-0002) |
37: | `templar-filed` | user | heavy I/O; a stuck mount must not stall policy |
38: | `templar-preview-codecs` | transient child of filed | bounded Templar-PDF render seam; keeps its heavier dependency graph out of the service |
39: | `templar-hostd` | root, system bus | the one privileged surface, one polkit policy (ADR-0003) |
40: | `templar-boot-health` | root, oneshot | gates `boot-complete.target` (ADR-0004) |
41: | `xdg-desktop-portal-templar` | user, D-Bus activated | one backend over the same services |
42: 
43: ## Crates → processes
44: 
45: | Crate | Hosted by | Contents |
46: |---|---|---|
47: | `templar-types` | everyone | AppId/InstanceId/manifests/claims, provider roles, unit naming, checkpoint protocol, D-Bus names |
48: | `templar-appd` | sessiond | Registry (PID→cgroup→instance→AppId), launcher, manifests, `AppIdentity1`/`Launcher1` |
49: | `templar-associationd` | sessiond | rule engine, MIME detector, `mimeapps.list` export, `Associations1` |
50: | `templar-session-core` | sessiond | providers, autostart, history, notifications, settings schema, projects, power, location, first-run |
51: | `templar-state` | sessiond, filed | SQLite migrations |
52: | `templar-window` | templar-comp | geometry invariant (`repair_window ⇒ is_recoverable`, proptest), window records, drag, layers — **no async deps** |
53: | `templar-field` | shell, sessiond | the Home Field model: tiles on a bounded grid, ops with exact inverses (undo = apply the inverse), stable local reflow, representation ladder, drag/throw state machine, text form — **no async deps, no toolkit** |
54: | `templar-treemap` | shell | cushion treemaps for tray and drawer interiors (file-tree arena, byte-proportional squarified layout, category palette, CPU rasterizer; lifted from Silvaview-rs) — **no dependencies at all** |
55: | `templar-file-index` | filed, shell, cli | the shared file index: generations, lossless byte paths, budgeted scans, FTS5 trigram search, cursors, sealed snapshots, and the `EventSource` watcher seam |
56: | `templar-fanotify` | nothing (measured, unadopted) | fanotify bindings and a `FanotifySource` behind `templar-file-index`'s `EventSource`. The one crate that does not `forbid(unsafe_code)`; `src/sys.rs` holds the exception and `crates/templar-fanotify/README.md` states why. `InotifySource` remains the default — see `docs/measurements/fanotify-2026-09-04.md` |
57: | `templar-comp-integration` | templar-comp | `IdentityCache` over D-Bus, governor notifier, shell broadcaster |
58: | `templar-comp` (bin) | itself | the Smithay compositor: DRM/KMS + winit backends, protocols, render loop (ADR-0012, GPL-3.0) |
59: | `templar-client` | shell, apps, portal, cli, comp | proxies + app SDK (checkpoint protocol) |
60: | `templar-hwdetect` | hostd, installer, doctor | GPU detection, flavour, modes |
61: | `templar-resourced` (lib) | itself | cgroup wrapper, PSI, pure `Governor`, executor, `app.slice` scan |
```

<a id="e02"></a>
## E02 — The shipped portal selection defaults to Templar and names Cosmic capture backends

**`TemplarOS/portals/xdg-desktop-portal-templar/templaros-portals.conf:1–6`**

```text
1: # Portal backend selection for the TemplarOS session (XDG_CURRENT_DESKTOP=TemplarOS).
2: # `templar-sessiond` regenerates the user copy from the `portal-backend` provider role.
3: [preferred]
4: default=templaros
5: org.freedesktop.impl.portal.ScreenCast=cosmic
6: org.freedesktop.impl.portal.Screenshot=cosmic
```

**`TemplarOS/portals/xdg-desktop-portal-templar/src/main.rs:1–20`**

```text
1: //! `xdg-desktop-portal-templar` — one backend, all clients of the same services:
2: //! FileChooser → `org.templaros.Files` (shared last-directory policy), AppChooser →
3: //! `org.templaros.Associations1` + `org.templaros.AppIdentity1`, Settings → `org.templaros.Settings1`,
4: //! Background → `AppIdentity1` + `Resources1`.
5: //!
6: //! Four interfaces, and only those four: the ones `templaros.portal` claims. **There is no OpenURI
7: //! backend here, and there is no such thing to write** — `org.freedesktop.impl.portal.OpenURI`
8: //! is not an interface the portal frontend defines or calls. `xdg-desktop-portal` implements
9: //! `org.freedesktop.portal.OpenURI` itself: it finds the default handler through
10: //! GAppInfo/`mimeapps.list` (which `templar-associationd` generates), asks *our* `AppChooser` only
11: //! when it has to ask, and then launches the chosen desktop entry in its own scope. So "open
12: //! this downloaded file" is served here by AppChooser, and the agreement between TemplarOS
13: //! defaults and what the frontend picks lives in that exported `mimeapps.list`.
14: //!
15: //! Real freedesktop portal interfaces, matched against the actual specification (fetched from
16: //! `flatpak/xdg-desktop-portal`'s `data/org.freedesktop.impl.portal.*.xml`), not new
17: //! `org.templaros.*` XML — see each interface module for what is genuinely implemented versus a
18: //! modelled seam (`file_chooser::FileChooserUi`, `app_chooser::AppChooserUi`,
19: //! `background::BackgroundUi`), each of which has a native Pixelkit implementation used whenever
20: //! there is a display to show it on.
```

<a id="e03"></a>
## E03 — Native FileChooser launches its graphical host on a blocking worker and discards a failed join

**`TemplarOS/portals/xdg-desktop-portal-templar/src/file_chooser.rs:39–89`**

```text
39: /// Everything a [`FileChooserUi`] needs to actually show something.
40: #[derive(Clone, Debug, PartialEq, Eq)]
41: pub struct ChooserRequest {
42:     pub kind: ChooserKind,
43:     pub app_id: String,
44:     pub title: String,
45:     pub starting_directory: PathBuf,
46:     /// The caller's `filters`, in the order offered.
47:     pub filters: Vec<FileFilter>,
48:     /// Which of `filters` starts selected; `None` shows everything.
49:     pub current_filter: Option<usize>,
50: }
51: 
52: /// What the user picked.
53: #[derive(Clone, Debug, Default, PartialEq, Eq)]
54: pub struct ChosenFiles {
55:     pub uris: Vec<String>,
56:     pub writable: bool,
57: }
58: 
59: /// The interactive part no toolkit exists to provide yet.
60: #[async_trait]
61: pub trait FileChooserUi: Send + Sync + std::fmt::Debug {
62:     async fn choose(&self, request: ChooserRequest) -> Option<ChosenFiles>;
63: }
64: 
65: /// Headless default: there is nothing to show, so nothing is chosen.
66: #[derive(Debug, Default)]
67: pub struct NoUiAvailable;
68: 
69: #[async_trait]
70: impl FileChooserUi for NoUiAvailable {
71:     async fn choose(&self, _request: ChooserRequest) -> Option<ChosenFiles> {
72:         None
73:     }
74: }
75: 
76: /// Pixelkit graphical chooser used by the live portal backend.
77: #[derive(Debug, Default)]
78: pub struct NativeUi;
79: 
80: #[async_trait]
81: impl FileChooserUi for NativeUi {
82:     async fn choose(&self, request: ChooserRequest) -> Option<ChosenFiles> {
83:         tokio::task::spawn_blocking(move || crate::dialog::files(request))
84:             .await
85:             .ok()
86:             .flatten()
87:     }
88: }
89: 
```

**`TemplarOS/portals/xdg-desktop-portal-templar/src/dialog.rs:375–393`**

```text
375: fn run(model: Model) -> Option<Answer> {
376:     let answer = Arc::clone(&model.answer);
377:     let app = DialogApp {
378:         model,
379:         surface: Surface::new(),
380:     };
381:     if let Err(error) = run_app(
382:         app,
383:         WindowConfig::new("TemplarOS portal", 720.0, 560.0).min_size(560.0, 440.0),
384:     ) {
385:         tracing::error!(%error, "portal dialog failed");
386:         return None;
387:     }
388:     answer.lock().expect("dialog answer").clone()
389: }
390: 
391: pub fn choice(title: String, message: String, choices: Vec<String>) -> Option<String> {
392:     if choices.is_empty() {
393:         return None;
```

<a id="e04"></a>
## E04 — PixelKit creates a fresh default winit loop for each host run

**`PixelKit/crates/pixelkit-windowing/src/host.rs:1134–1200`**

```text
1134: pub fn run_app<A: PixelApp + 'static>(
1135:     app: A,
1136:     config: WindowConfig,
1137: ) -> Result<(), Box<dyn std::error::Error>> {
1138:     run_app_with_presenter(app, config, None)
1139: }
1140: 
1141: /// `presenter` replaces the softbuffer default. The closure receives the
1142: /// window once, when it is created.
1143: pub fn run_app_with_presenter<A: PixelApp + 'static>(
1144:     app: A,
1145:     config: WindowConfig,
1146:     presenter: Option<PresenterFactory>,
1147: ) -> Result<(), Box<dyn std::error::Error>> {
1148:     run_inner(vec![Slot::new(Box::new(app), config, presenter)], None)
1149: }
1150: 
1151: /// Run several windows, each with its own application, sharing one event
1152: /// loop. Every window behaves like [`run_app`]'s: its own buffer, presenter,
1153: /// scale and timers, with input routed to it by its window id.
1154: ///
1155: /// Closing one window's close box closes that window; the loop ends when the
1156: /// last one goes, or when any application votes [`PixelApp::should_exit`].
1157: /// An empty list returns `Ok` immediately: there is nothing to show.
1158: pub fn run_windows(
1159:     apps: Vec<(WindowConfig, Box<dyn PixelApp>)>,
1160: ) -> Result<(), Box<dyn std::error::Error>> {
1161:     run_windows_with_presenter(apps, None)
1162: }
1163: 
1164: /// [`run_windows`] with a presenter factory shared by every window.
1165: /// `presenter` replaces the softbuffer default for the initial windows and
1166: /// for any window an application opens later at runtime.
1167: pub fn run_windows_with_presenter(
1168:     apps: Vec<(WindowConfig, Box<dyn PixelApp>)>,
1169:     presenter: Option<MultiPresenterFactory>,
1170: ) -> Result<(), Box<dyn std::error::Error>> {
1171:     let slots = apps
1172:         .into_iter()
1173:         .map(|(config, app)| Slot::new(app, config, None))
1174:         .collect();
1175:     run_inner(slots, presenter)
1176: }
1177: 
1178: fn run_inner(
1179:     slots: Vec<Slot>,
1180:     shared_presenter: Option<MultiPresenterFactory>,
1181: ) -> Result<(), Box<dyn std::error::Error>> {
1182:     let event_loop = EventLoop::<Host>::with_user_event().build()?;
1183:     let mut shell = Shell {
1184:         slots,
1185:         shared_presenter,
1186:         #[cfg(feature = "accessibility")]
1187:         proxy: event_loop.create_proxy(),
1188:     };
1189:     for slot in &mut shell.slots {
1190:         slot.app.attach(Waker {
1191:             proxy: Some(event_loop.create_proxy()),
1192:         });
1193:     }
1194:     event_loop.run_app(&mut shell)?;
1195:     Ok(())
1196: }
1197: 
1198: #[cfg(test)]
1199: mod tests {
1200:     use super::*;
```

<a id="e05"></a>
## E05 — Portal parenting is not carried into the native chooser model

**`TemplarOS/portals/xdg-desktop-portal-templar/src/file_chooser.rs:285–334`**

```text
285:         }
286:     }
287: }
288: 
289: #[zbus::interface(name = "org.freedesktop.impl.portal.FileChooser")]
290: impl FileChooserApi {
291:     /// `parent_window` is part of the fixed spec signature on every method here; this backend
292:     /// needs no window-parenting (no toolkit dialog exists yet — see `FileChooserUi`).
293:     #[zbus(name = "OpenFile")]
294:     #[allow(unused_variables)]
295:     async fn open_file(
296:         &self,
297:         #[zbus(connection)] connection: &zbus::Connection,
298:         handle: OwnedObjectPath,
299:         app_id: &str,
300:         parent_window: &str,
301:         title: &str,
302:         options: HashMap<String, OwnedValue>,
303:     ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
304:         let kind = ChooserKind::Open {
305:             multiple: get_bool(&options, "multiple"),
306:             directory: get_bool(&options, "directory"),
307:         };
308:         with_request(connection, &handle, || {
309:             self.run(kind, app_id, title, &options)
310:         })
311:         .await
312:     }
313: 
314:     #[zbus(name = "SaveFile")]
315:     #[allow(unused_variables)]
316:     async fn save_file(
317:         &self,
318:         #[zbus(connection)] connection: &zbus::Connection,
319:         handle: OwnedObjectPath,
320:         app_id: &str,
321:         parent_window: &str,
322:         title: &str,
323:         options: HashMap<String, OwnedValue>,
324:     ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
325:         let kind = ChooserKind::Save {
326:             current_name: get_string(&options, "current_name").or_else(|| {
327:                 get_bytes(&options, "current_file")
328:                     .map(path_from_bytes)
329:                     .and_then(|file| {
330:                         file.file_name()
331:                             .map(|name| name.to_string_lossy().into_owned())
332:                     })
333:             }),
334:         };
```

<a id="e06"></a>
## E06 — The DRM backend explicitly rejects non-primary GPUs

**`TemplarOS/compositor/templar-comp/src/backend/udev.rs:1–18`**

```text
1: //! The udev/KMS backend: real hardware, via `LibSeatSession` and atomic-modesetting DRM. Modelled
2: //! closely on `anvil/src/udev.rs` (MIT, ADR-0012) with its multi-GPU render-node juggling,
3: //! VRR/direct-scanout and debug-overlay machinery stripped out — this crate targets one primary
4: //! GPU correctly, not every GPU configuration anvil handles (see [`is_primary_gpu_dev`]: every
5: //! other DRM device udev reports, e.g. the NVIDIA half of a hybrid Intel+NVIDIA laptop, is logged
6: //! and ignored rather than juggled). It cannot be exercised or hardware-tested from the
7: //! development session this crate was written in (no DRM device this session can take
8: //! modesetting master on); it is written to be correct by construction against the pinned
9: //! Smithay API and to fail loudly (a logged error, not a panic that takes the session down) if a
10: //! step it depends on is unavailable, rather than run.
11: //!
12: //! Every output-topology-affecting event below funnels through the same
13: //! `crate::backend::recompute_outputs` entry point winit uses — connector hotplug, mode changes
14: //! and session resume all call it, never touching `WindowService` directly (COMPOSITOR.md's "the
15: //! only place `Output`/topology changes enter `templar-window`").
16: //!
17: //! Rendering is driven entirely by page-flip completion: [`add_device`] creates one
18: //! [`DrmOutput`]/[`DrmOutputManager`] surface per connected CRTC (`connector_connected`), each
```

**`TemplarOS/compositor/templar-comp/src/backend/udev.rs:519–539`**

```text
519: 
520: // Long and linear for the same reason `run` is (see its `#[allow]`): this is "open one device and
521: // wire up everything it needs," not several independent concerns artificially split apart.
522: #[allow(clippy::too_many_lines)]
523: fn add_device(state: &mut State, node: DrmNode, path: &std::path::Path) {
524:     if !is_primary_gpu_dev(node.dev_id(), state.backend_data.primary_card_dev_id) {
525:         tracing::info!(
526:             ?node,
527:             ?path,
528:             "ignoring non-primary GPU device (single-primary-GPU scope, see module doc)"
529:         );
530:         return;
531:     }
532:     if state.backend_data.devices.contains_key(&node) {
533:         tracing::debug!(?node, "drm device already tracked, ignoring duplicate add");
534:         return;
535:     }
536: 
537:     let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
538:     let Ok(fd) = state.backend_data.session.open(path, open_flags) else {
539:         tracing::warn!(?path, "failed to open drm device");
```

<a id="e07"></a>
## E07 — Output initialization selects one preferred mode and uses a placeholder serial

**`TemplarOS/compositor/templar-comp/src/backend/udev.rs:693–751`**

```text
693:     let output_name = output_name(connector.interface(), connector.interface_id());
694:     let info = display_info::for_connector(device.drm_output_manager.device(), connector.handle());
695:     // `libdisplay_info::info::Info` (the closures' receiver type) isn't nameable here without
696:     // adding `libdisplay-info` as a direct dependency purely to write `Info::make`/`Info::model`
697:     // in method-reference form — not worth a new dependency for a clippy style preference.
698:     #[allow(clippy::redundant_closure_for_method_calls)]
699:     let make = info
700:         .as_ref()
701:         .and_then(|info| info.make())
702:         .unwrap_or_else(|| "Unknown".to_owned());
703:     #[allow(clippy::redundant_closure_for_method_calls)]
704:     let model = info
705:         .as_ref()
706:         .and_then(|info| info.model())
707:         .unwrap_or_else(|| "Unknown".to_owned());
708: 
709:     let Some(drm_mode) = select_preferred_mode(connector.modes(), |m| {
710:         m.mode_type()
711:             .contains(smithay::reexports::drm::control::ModeTypeFlags::PREFERRED)
712:     }) else {
713:         tracing::warn!(%output_name, "connector reports no usable mode, leaving it dark");
714:         return;
715:     };
716:     let wl_mode = Mode::from(drm_mode);
717: 
718:     let x = next_output_x(
719:         state
720:             .space
721:             .outputs()
722:             .filter_map(|o| state.space.output_geometry(o))
723:             .map(|geo| geo.loc.x + geo.size.w),
724:     );
725: 
726:     let (phys_w, phys_h) = connector.size().unwrap_or((0, 0));
727:     let output = Output::new(
728:         output_name.clone(),
729:         PhysicalProperties {
730:             size: (phys_w as i32, phys_h as i32).into(),
731:             subpixel: connector.subpixel().into(),
732:             make,
733:             model,
734:             serial_number: "1".into(),
735:         },
736:     );
737:     let global = output.create_global::<State>(&state.display_handle);
738:     output.change_current_state(
739:         Some(wl_mode),
740:         Some(Transform::Normal),
741:         None,
742:         Some((x, 0).into()),
743:     );
744:     output.set_preferred(wl_mode);
745:     state.space.map_output(&output, (x, 0));
746: 
747:     let Some(device) = state.backend_data.devices.get_mut(&node) else {
748:         state.space.unmap_output(&output);
749:         state.display_handle.remove_global::<State>(global);
750:         return;
751:     };
```

**`TemplarOS/compositor/templar-comp/src/state.rs:372–395`**

```text
372:             loop_signal,
373:             clock: Clock::new(),
374:             start_time: std::time::Instant::now(),
375: 
376:             compositor_state: CompositorState::new::<Self>(dh),
377:             shm_state: ShmState::new::<Self>(dh, Vec::new()),
378:             output_manager_state: OutputManagerState::new_with_xdg_output::<Self>(dh),
379:             seat_state,
380:             data_device_state: DataDeviceState::new::<Self>(dh),
381:             primary_selection_state,
382:             data_control_state,
383:             viewporter_state: ViewporterState::new::<Self>(dh),
384:             fractional_scale_manager_state: FractionalScaleManagerState::new::<Self>(dh),
385:             presentation_state: PresentationState::new::<Self>(dh, libc::CLOCK_MONOTONIC as u32),
386:             xdg_shell_state: XdgShellState::new::<Self>(dh),
387:             xdg_decoration_state: XdgDecorationState::new::<Self>(dh),
388:             xdg_dialog_state: XdgDialogState::new::<Self>(dh),
389:             xdg_activation_state: XdgActivationState::new::<Self>(dh),
390:             session_lock_manager_state: SessionLockManagerState::new::<Self, _>(dh, |_| true),
391:             idle_inhibit_manager_state:
392:                 smithay::wayland::idle_inhibit::IdleInhibitManagerState::new::<Self>(dh),
393:             foreign_toplevel_list_state:
394:                 smithay::wayland::foreign_toplevel_list::ForeignToplevelListState::new::<Self>(dh),
395:             dmabuf_state: smithay::wayland::dmabuf::DmabufState::new(),
```

<a id="e08"></a>
## E08 — Settings has existing operational pages but not the proposed ordinary hardware navigation

**`TemplarOS/apps/templar-settings/src/surface.rs:30–65`**

```text
30: /// outright. The rest keep the order they have had, so a person who learned where a tab is does
31: /// not have to learn it again.
32: /// Appearance is last rather than beside Home Field for the same reason: it is new, and moving
33: /// six tabs to make room for it would cost every person who already knows where History is.
34: /// Homegarden is appended rather than placed beside Home Field for the same reason Appearance
35: /// was: it is new, and moving nine tabs to make room for it would cost every person who already
36: /// knows where History is. Rails is appended for the same reason again.
37: const PAGE_NAMES: [&str; 11] = [
38:     "Home Field",
39:     "System",
40:     "Providers",
41:     "Associations",
42:     "Background",
43:     "Resources",
44:     "History",
45:     "Hardware",
46:     "Appearance",
47:     "Homegarden",
48:     "Rails",
49: ];
50: /// The index of each page that draws controls rather than rendered text.
51: const HOME_PAGE: usize = 0;
52: const SYSTEM_PAGE: usize = 1;
53: const PROVIDERS_PAGE: usize = 2;
54: const ASSOCIATIONS_PAGE: usize = 3;
55: const BACKGROUND_PAGE: usize = 4;
56: const HISTORY_PAGE: usize = 6;
57: const APPEARANCE_PAGE: usize = 8;
58: const GARDEN_PAGE: usize = 9;
59: const RAILS_PAGE: usize = 10;
60: /// How far one press of a stepper moves a level: the same five points `templar status louder` and
61: /// the Home Field's own controls use, so no two surfaces disagree about what one step means.
62: const STEP_PERCENT: i32 = 5;
63: /// How many recent events the History page holds. A settings page is read to find something that
64: /// happened today, not to audit a month, and every row is parsed and laid out on the UI thread.
65: const HISTORY_LIMIT: u32 = 200;
```

<a id="e09"></a>
## E09 — Audio is presently a bounded-scope wpctl adapter, not a complete device selector

**`TemplarOS/services/templar-sessiond/src/apis/audio.rs:1–76`**

```text
1: //! `org.templaros.Audio1`: the default sink's volume, through WirePlumber's `wpctl`.
2: //!
3: //! **Why a subprocess and not libpipewire.** The native route means linking a C library, needing
4: //! its headers at build time, and writing unsafe FFI in a workspace that forbids it — and it
5: //! means a PipeWire main loop living inside the one policy daemon whose crash takes identity,
6: //! launch, notifications and settings with it (ADR-0001). `wpctl` ships with WirePlumber on every
7: //! PipeWire desktop, runs with no display and no audio session of its own, and costs one short
8: //! process per adjustment. A volume key is pressed a few times a minute, not a few thousand times
9: //! a second, so that cost buys a dependency-free implementation that cannot take the daemon down.
10: //!
11: //! The trade is stated plainly in the interface XML: without a PipeWire event subscription, a
12: //! change made *elsewhere* — a hardware key wired straight to WirePlumber — is not observed, so
13: //! `Changed` covers changes made through this interface and clients that must not go stale poll.
14: //!
15: //! Everything below is testable without touching the machine's audio: [`parse_volume`] is a pure
16: //! function over `wpctl`'s real output, and the command runner is a trait, so a test observes the
17: //! `wpctl` invocation that *would* have been made rather than making it.
18: 
19: use crate::apis::failed;
20: use anyhow::{Context, Result, bail};
21: use async_trait::async_trait;
22: use serde::{Deserialize, Serialize};
23: use std::sync::Arc;
24: use zbus::fdo;
25: 
26: /// WirePlumber's name for "whatever the user is listening through".
27: const DEFAULT_SINK: &str = "@DEFAULT_AUDIO_SINK@";
28: 
29: /// What `Audio1.Get` answers.
30: #[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
31: pub struct AudioState {
32:     /// Whether this machine has a volume this service can reach: WirePlumber present, and a
33:     /// default sink to point at. A machine with no audio is a fact, not a failure.
34:     pub present: bool,
35:     pub volume_percent: u32,
36:     pub muted: bool,
37: }
38: 
39: /// What this interface needs from WirePlumber.
40: #[async_trait]
41: pub trait AudioControl: Send + Sync + std::fmt::Debug {
42:     /// Run `wpctl` with these arguments, returning its stdout.
43:     ///
44:     /// One method rather than one per operation, so the fake in the tests observes the exact
45:     /// command line and the parsing under test is the same code the daemon runs.
46:     async fn wpctl(&self, arguments: &[&str]) -> Result<String>;
47: }
48: 
49: /// The real thing: `wpctl` on `PATH`.
50: #[derive(Debug, Default)]
51: pub struct WpctlAudio;
52: 
53: #[async_trait]
54: impl AudioControl for WpctlAudio {
55:     async fn wpctl(&self, arguments: &[&str]) -> Result<String> {
56:         let arguments: Vec<String> = arguments.iter().map(|value| (*value).to_owned()).collect();
57:         let output = tokio::process::Command::new("wpctl")
58:             .args(&arguments)
59:             .output()
60:             .await
61:             .with_context(|| format!("running wpctl {}", arguments.join(" ")))?;
62:         if !output.status.success() {
63:             bail!(
64:                 "wpctl {} failed: {}",
65:                 arguments.join(" "),
66:                 String::from_utf8_lossy(&output.stderr).trim()
67:             );
68:         }
69:         Ok(String::from_utf8_lossy(&output.stdout).into_owned())
70:     }
71: }
72: 
73: /// Parse `wpctl get-volume @DEFAULT_AUDIO_SINK@`, whose output is one line:
74: /// `Volume: 0.45` or `Volume: 0.45 [MUTED]`.
75: ///
76: /// A pure function over the real format, so the shape `wpctl` actually prints is what the tests
```

<a id="e10"></a>
## E10 — Power policy comments are partly stale; manual lock is now wired

**`TemplarOS/crates/templar-session-core/src/power.rs:1–23`**

```text
1: //! The power policy a session *records*: what should happen after an idle interval, and what a
2: //! closing lid should do, for each power source.
3: //!
4: //! Pure data. Nothing in *this crate* enforces any of it, and only one field is enforced
5: //! anywhere: [`PowerProfile::screen_off_after`] is read by the shell and relayed to the
6: //! compositor, which turns the outputs off after that much idle time and back on at the next
7: //! input (`compositor/templar-comp/src/idle.rs`). The compositor is the only process that can see
8: //! the seat go quiet, so that is where it had to live.
9: //!
10: //! Every other field is still a value stored and not a thing that happens: there is no lid
11: //! listener and no call to logind or upower anywhere behind this, and setting `suspend_after` has
12: //! never suspended anything. Treating those as a description of what the machine will do is the
13: //! one mistake this module invites, and the reason the sentence is here.
14: //!
15: //! The rest is deliberate remaining scope. `dim_after` needs a backlight ramp (now
16: //! `org.templaros.Backlight1`), `lock_after` needs a lock surface to drive — and
17: //! `Session.Lifecycle1.Lock` is still a stub for want of a Wayland lock-manager client. See
18: //! `services/templar-sessiond/src/apis/power.rs`.
19: //!
20: //! Battery *state* is not here and never was: that is `org.templaros.Battery1`, a hardware reading
21: //! rather than stored policy.
22: 
23: use serde::{Deserialize, Serialize};
```

**`TemplarOS/services/templar-sessiond/src/apis/lifecycle.rs:425–448`**

```text
425:     }
426: 
427:     /// Ask for the session to be locked.
428:     ///
429:     /// This daemon does not lock the screen itself and is not going to: the lock screen is
430:     /// compositor-owned (ADR-0010) and `ext-session-lock-v1` can only be driven by a Wayland
431:     /// client, which sessiond deliberately is not. So this emits [`LockRequested`], and
432:     /// `templar-shell` — which already holds the compositor's trusted-shell connection — is what
433:     /// calls `lock()` on the protocol.
434:     ///
435:     /// Returning `Ok` here means the request went out, not that the screen is covered. That is
436:     /// honest in a way the old `NotSupported` was not, and it is the only thing this process is
437:     /// in a position to promise.
438:     async fn lock(&self, #[zbus(signal_emitter)] emitter: SignalEmitter<'_>) -> fdo::Result<()> {
439:         // The edge is the request, not the covered screen — the same honesty the doc comment
440:         // above claims. A lock the compositor then refuses is followed by an unlock, because the
441:         // shell tears the attempt down through the same `release_lock` path.
442:         let _ = self.edges.send(SessionEdge::Locked);
443:         emitter
444:             .lock_requested()
445:             .await
446:             .map_err(|error| fdo::Error::Failed(format!("emitting LockRequested: {error}")))
447:     }
448: 
```

<a id="e11"></a>
## E11 — Lock acknowledgment is sent before the all-output presentation barrier

**`TemplarOS/compositor/templar-comp/src/session_lock.rs:33–49`**

```text
33: impl<B: Backend + 'static> SessionLockHandler for TemplarCompState<B> {
34:     fn lock_state(&mut self) -> &mut SessionLockManagerState {
35:         &mut self.session_lock_manager_state
36:     }
37: 
38:     fn lock(&mut self, confirmation: SessionLocker) {
39:         self.session_lock.locked = true;
40:         // Nothing of the session shows through the lock, a rail included.
41:         self.close_rail("session locked");
42:         // Smithay's contract: confirm only once a cleared frame has actually been presented on
43:         // every output. We have no in-flight-frame tracking to gate this on yet, so we confirm
44:         // immediately — the lock state (and the input-isolation guards in `src/input.rs`) is
45:         // already in effect before this returns, so nothing sensitive is exposed by confirming
46:         // eagerly; it only means a client screen-recording the exact lock transition could catch
47:         // one still-unlocked frame already in flight, not a real bypass of the lock itself.
48:         confirmation.lock();
49:     }
```

<a id="e12"></a>
## E12 — Non-macOS PixelKit clipboard is process-local, not the Linux desktop clipboard

**`PixelKit/crates/pixelkit-shell/src/clipboard.rs:1–59`**

```text
1: //! Plain text with the system clipboard.
2: //!
3: //! On macOS this is the general `NSPasteboard` behind safe objc2 wrappers
4: //! (no `unsafe` in this workspace): copying a phone number out of Messages
5: //! and pasting it into a field works, and copying out of a field works in
6: //! every other app. Anywhere else, a process-local fallback answers
7: //! instead. Reads return `None` — never an error — when there is no text,
8: //! which is also what a failing pasteboard looks like.
9: 
10: /// Process-local fallback where there is no system pasteboard.
11: #[cfg(not(target_os = "macos"))]
12: #[derive(Debug, Clone, Default)]
13: struct Fallback {
14:     text: Option<String>,
15: }
16: 
17: /// The system clipboard for plain text.
18: #[derive(Debug, Clone, Default)]
19: pub struct Clipboard {
20:     #[cfg(not(target_os = "macos"))]
21:     fallback: Fallback,
22: }
23: 
24: impl Clipboard {
25:     /// An empty clipboard handle. Handles are cheap and stateless; the
26:     /// pasteboard itself lives with the system.
27:     pub fn new() -> Clipboard {
28:         Clipboard::default()
29:     }
30: 
31:     /// Copy `text` onto the clipboard, replacing whatever was there.
32:     /// Returns whether the text reached the clipboard.
33:     pub fn set_text(&mut self, text: impl Into<String>) -> bool {
34:         let text = text.into();
35:         #[cfg(target_os = "macos")]
36:         {
37:             if macos::set(&text) {
38:                 return true;
39:             }
40:             return false;
41:         }
42:         #[cfg(not(target_os = "macos"))]
43:         {
44:             self.fallback.text = Some(text);
45:             true
46:         }
47:     }
48: 
49:     /// The clipboard's text, or `None` when there is none. Never fails.
50:     pub fn text(&self) -> Option<String> {
51:         #[cfg(target_os = "macos")]
52:         {
53:             return macos::get();
54:         }
55:         #[cfg(not(target_os = "macos"))]
56:         {
57:             self.fallback.text.clone()
58:         }
59:     }
```

<a id="e13"></a>
## E13 — PixelKit has optional accessibility hooks and a transitive windowing dependency in UI

**`PixelKit/crates/pixelkit-windowing/src/host.rs:248–294`**

```text
248:     /// kept separate from the pixel conversion.
249:     fn on_scroll_event(&mut self, _event: &ScrollEvent) {}
250: 
251:     /// Pinch / magnify. `delta` is the platform's magnification step.
252:     fn on_magnify(&mut self, _phase: GesturePhase, _delta: f64) {}
253: 
254:     /// Caret rectangle for the IME candidate window, in physical pixels.
255:     /// `None` leaves the platform's last rectangle alone.
256:     fn ime_cursor_area(&self) -> Option<ImeCursorArea> {
257:         None
258:     }
259: 
260:     /// Whether the application wants the windows closed. Checked after every
261:     /// tick, so an application can quit itself. In a multi-window run one
262:     /// vote closes everything; a window that only wants itself gone returns
263:     /// [`WindowRequest::Close`] from [`PixelApp::poll_window`] instead.
264:     fn should_exit(&self) -> bool {
265:         false
266:     }
267: 
268:     /// The window's system theme is `dark`. Called once at startup (if the
269:     /// platform reports one) and again on every change, so a screen can
270:     /// switch light/dark colours without polling.
271:     fn on_theme(&mut self, _dark: bool) {}
272: 
273:     /// The application's semantic tree for assistive technology, or `None`
274:     /// for an application that has none.
275:     ///
276:     /// Asked for once when a screen reader attaches and again after every
277:     /// paint while one is attached, so it describes the frame that was just
278:     /// drawn: the same widgets, with the same bounds in physical pixels.
279:     /// The toolkit keeps no widget identity — that is the immediate-mode
280:     /// bargain — so the ids in the tree are the application's own, chosen
281:     /// from what a control means rather than where it sits.
282:     #[cfg(feature = "accessibility")]
283:     fn accessibility_tree(&mut self) -> Option<accesskit::TreeUpdate> {
284:         None
285:     }
286: 
287:     /// Assistive technology asked for an action on a node the application
288:     /// published, such as a click on a button.
289:     #[cfg(feature = "accessibility")]
290:     fn on_accessibility_action(&mut self, _request: accesskit::ActionRequest) {}
291: }
292: 
293: /// A window an application wants opened or closed. See
294: /// [`PixelApp::poll_window`].
```

**`PixelKit/crates/pixelkit-ui/Cargo.toml:1–21`**

```text
1: [package]
2: name = "pixelkit-ui"
3: version.workspace = true
4: edition.workspace = true
5: rust-version.workspace = true
6: license.workspace = true
7: 
8: [dependencies]
9: pixelkit-raster = { workspace = true }
10: pixelkit-text = { workspace = true }
11: pixelkit-shell = { workspace = true }
12: 
13: [dev-dependencies]
14: pixelkit-text = { workspace = true, features = ["test-fonts"] }
15: pixelkit-windowing = { workspace = true }
16: 
17: [features]
18: test-fonts = ["pixelkit-text/test-fonts"]
19: 
20: [lints]
21: workspace = true
```

<a id="e14"></a>
## E14 — PCManFM integration exists; the bookmark export currently overwrites the entire compatibility file

**`TemplarOS/crates/templar-session-core/src/providers.rs:152–168`**

```text
152: const DEFAULT_FILE_SEARCH: &str = "fsearch";
153: 
154: /// The terminal TemplarOS ships with, by executable name. It wins by priority when it is installed,
155: /// exactly as a distribution default is supposed to: installing a second terminal does not
156: /// silently take the role, and choosing one in Settings still overrules both.
157: const DEFAULT_TERMINAL: &str = "kitty";
158: 
159: /// The freedesktop category every file manager carries, and the one TemplarOS ships. A file manager
160: /// declares no more about TemplarOS than a terminal does, so the role is read off `Categories=` the
161: /// same way — and `pcmanfm-qt` wins by priority for the same reason kitty does.
162: const FILE_MANAGER_CATEGORY: &str = "FileManager";
163: const DEFAULT_FILE_MANAGER: &str = "pcmanfm-qt";
164: 
165: /// The roles an installed application fills by *being what it is*, rather than by declaring them.
166: ///
167: /// Every other exclusive role is claimed: an application says `X-TemplarOS-Provides=lock-screen` in
168: /// its entry, or calls `Providers1.Declare`. A terminal emulator claims nothing — it is an
```

**`TemplarOS/services/templar-filed/src/bookmarks.rs:1–76`**

```text
1: //! Per-user folder bookmarks, shared by every Files window. Mutations are atomic and never
2: //! change the referenced directory.
3: //!
4: //! filed is the authority, and the GTK bookmarks file is a one-way export of it. pcmanfm-qt's
5: //! Places sidebar (libfm-qt) reads `$XDG_CONFIG_HOME/gtk-3.0/bookmarks`, and that is the only
6: //! route it offers — there is no plugin API to teach it about `Files.Browse1` — so every mutation
7: //! rewrites that file. Nothing reads it back: importing would make two writers of one list and
8: //! there would be no answer to which one is right.
9: use anyhow::{Context, Result, ensure};
10: use std::fmt::Write as _;
11: use std::path::{Path, PathBuf};
12: 
13: #[derive(Clone, Debug)]
14: pub struct Bookmarks(pub templar_state::Store);
15: 
16: impl Bookmarks {
17:     pub fn set(&self, path: &Path, bookmarked: bool) -> Result<()> {
18:         ensure!(
19:             path.is_absolute(),
20:             "A bookmark needs an absolute folder path"
21:         );
22:         let text = path
23:             .to_str()
24:             .ok_or_else(|| anyhow::anyhow!("Path cannot be represented on this interface"))?;
25:         if bookmarked {
26:             ensure!(path.is_dir(), "Only folders can be bookmarked");
27:             let label = path
28:                 .file_name()
29:                 .unwrap_or(path.as_os_str())
30:                 .to_string_lossy();
31:             self.0.lock().execute(
32:                 "INSERT INTO folder_bookmarks(path,label,at_ms) VALUES (?1,?2,?3) ON CONFLICT(path) DO NOTHING",
33:                 rusqlite::params![text, label, templar_state::now_ms()],
34:             )?;
35:         } else {
36:             self.0
37:                 .lock()
38:                 .execute("DELETE FROM folder_bookmarks WHERE path=?1", [text])?;
39:         }
40:         // Best-effort: a read-only or absent home must not fail a bookmark that the store already
41:         // committed. The export is a convenience for another application, not part of the write.
42:         if let Some(file) = gtk_bookmarks_path()
43:             && let Err(error) = self.export_gtk_bookmarks(&file)
44:         {
45:             tracing::warn!(%error, path = %file.display(), "exporting GTK bookmarks");
46:         }
47:         Ok(())
48:     }
49: 
50:     /// Rewrite `file` as the GTK bookmarks list: one `file://` URI per line, with the label after
51:     /// a space when it differs from the folder's own name (the form GTK and libfm-qt both parse).
52:     pub fn export_gtk_bookmarks(&self, file: &Path) -> Result<()> {
53:         let mut text = String::new();
54:         for root in self.roots()? {
55:             let Some(path) = root.path.to_str() else {
56:                 continue;
57:             };
58:             let _ = write!(text, "file://{}", uri_encode(path));
59:             if root
60:                 .path
61:                 .file_name()
62:                 .is_some_and(|name| name != root.label.as_str())
63:             {
64:                 text.push(' ');
65:                 text.push_str(&root.label);
66:             }
67:             text.push('\n');
68:         }
69:         if let Some(parent) = file.parent() {
70:             std::fs::create_dir_all(parent)
71:                 .with_context(|| format!("creating {}", parent.display()))?;
72:         }
73:         std::fs::write(file, text).with_context(|| format!("writing {}", file.display()))
74:     }
75: 
76:     pub fn roots(&self) -> Result<Vec<crate::browse::BrowserRoot>> {
```

<a id="e15"></a>
## E15 — Real hostd staging uses a state-directory path and serializes the mounted path as subvolume

**`TemplarOS/services/templar-hostd/src/main.rs:43–69`**

```text
43: /// the boot manager ADR-0004 auto-detects from the installation's actual loader, and the real
44: /// polkit authority over the same system-bus connection hostd serves on.
45: fn real_config(root: &std::path::Path, connection: zbus::Connection) -> HostdConfig {
46:     let commands: Arc<dyn cmd::CommandRunner> = Arc::new(cmd::RealCommandRunner::default());
47:     let state_dir = root.join("var/lib/templaros/hostd");
48:     HostdConfig {
49:         apt: Arc::new(apt::SystemdRunApt {
50:             commands: commands.clone(),
51:         }),
52:         snapshotter: Arc::new(snapshot::BtrfsSnapshotter {
53:             commands: commands.clone(),
54:         }),
55:         boot: deploy::detect_backend(root, commands.clone(), state_dir.join("grub")).into(),
56:         commands,
57:         clock: Arc::new(clock::SystemClock),
58:         polkit: Arc::new(polkit::SystemPolkit::new(connection.clone())),
59:         journal_dir: state_dir.join("transactions"),
60:         source_root: root.to_path_buf(),
61:         // Candidate snapshots live alongside the running deployment on the same btrfs volume;
62:         // where exactly that top-level subvolume is mounted is an image-layout question
63:         // (`distro/deploy/README.md`), not one this daemon can discover on its own, so it is a
64:         // configurable default rather than a hardcoded assumption about `/`.
65:         deployments_dir: state_dir.join("deployments"),
66:         esp_root: deploy::esp_root(root),
67:         deployments_path: state_dir.join("deployments.json"),
68:         sys_root: root.join("sys"),
69:         apt_root: root.to_path_buf(),
```

**`TemplarOS/services/templar-hostd/src/updates.rs:307–335`**

```text
307:             // btrfs subvolume snapshot, none of these operations refuses to write over what is
308:             // already there.
309:             let candidate = self.candidate_dir(&txn.id);
310:             let result = verify_candidate(&candidate, &txn.changes)
311:                 .and_then(|()| self.snapshotter.seal_read_only(&candidate))
312:                 .and_then(|()| {
313:                     deploy::place_boot_files(&candidate, &self.esp_root, &txn.id).map(|_| ())
314:                 })
315:                 .and_then(|()| {
316:                     let deployment = deploy::Deployment {
317:                         id: txn.id.clone(),
318:                         subvolume: candidate.display().to_string(),
319:                         esp_dir: self
320:                             .esp_root
321:                             .join("EFI/templaros")
322:                             .join(&txn.id)
323:                             .display()
324:                             .to_string(),
325:                         kernel: txn.changes.kernel.clone(),
326:                         nvidia_modules: txn.changes.nvidia_modules.clone(),
327:                         known_good: false,
328:                     };
329:                     self.boot.stage_candidate(&deployment, 3)
330:                 });
331:             match result {
332:                 Ok(()) => {
333:                     txn.advance(Phase::AwaitingReboot, "verified, sealed, staged for boot")
334:                         .map_err(anyhow::Error::msg)?;
335:                     self.persist(&txn)?;
```

**`TemplarOS/services/templar-hostd/src/boot_entries.rs:205–224`**

```text
205:     for entry in entries {
206:         if !is_valid_deployment_id(&entry.id) {
207:             bail!(
208:                 "refusing to write a GRUB entry for invalid deployment id {:?}",
209:                 entry.id
210:             );
211:         }
212:         if entry.subvolume.is_empty()
213:             || !entry.subvolume.starts_with('@')
214:             || !entry
215:                 .subvolume
216:                 .chars()
217:                 .all(|c| c.is_ascii_alphanumeric() || matches!(c, '@' | '/' | '.' | '_' | '-'))
218:         {
219:             bail!("refusing unsafe root subvolume {:?}", entry.subvolume);
220:         }
221:     }
222:     let path = boot_root.join(ENTRIES_FILE);
223:     let parent = path
224:         .parent()
```

<a id="e16"></a>
## E16 — The declared storage layout still groups /var separately and expects @deployments

**`TemplarOS/distro/deploy/layout.toml:34–51`**

```text
34: [root]
35: type = "root"
36: format = "btrfs"
37: size_min = "8G"
38: # Set by the provisioner (`mkfs.btrfs -L`). This one *is* load-bearing: every generated boot
39: # entry says `root=LABEL=templaros-root`, because the label is identical for every deployment and
40: # the deployment id on the command line is what tells them apart.
41: label = "templaros-root"
42: mount = "/"
43: mount_options = "compress=zstd:1"
44: # @deployments holds the read-only candidate snapshots the update engine stages
45: # (services/templar-hostd/src/updates.rs). It is authoritative: the declaration follows the engine.
46: subvolumes = ["/@", "/@home", "/@var", "/@snapshots", "/@deployments"]
47: 
48: # --- An optional independent data role ---------------------------------------------------------
49: 
50: # A second filesystem that is not the OS and is never assumed expendable
51: # (@templaros.decision.independent-storage-roles). Absent from every machine provisioned so far; the
```

**`TemplarOS/distro/deploy/layout.toml:93–120`**

```text
93: [deployment.cmdline]
94: # The one thing that distinguishes deployments after the fact. Read by
95: # `state::booted_deployment_from_cmdline`; written identically by both boot backends.
96: key = "templaros.deployment="
97: template = "root=LABEL=templaros-root rootflags=subvol=@ rw templaros.deployment={id}"
98: 
99: # --- Boot managers ------------------------------------------------------------------------------
100: 
101: # Bare metal (Developer Edition 0.1): Canonical-signed shim -> Canonical-signed GRUB ->
102: # Ubuntu-signed kernel -> Canonical-signed matching NVIDIA LRM. No TemplarOS signing infrastructure
103: # and no MOK enrolment.
104: [boot.grub]
105: # Written whole by hostd on every stage/promote/rollback. Never hand-edited, never produced by
106: # grub-mkconfig.
107: entries_file = "/boot/grub/templaros-entries.cfg"
108: # Shipped as distro/deploy/grub.d/09_templaros, installed by the provisioner. It makes grub.cfg
109: # source the generated file at boot, so hostd never has to run grub-mkconfig.
110: stub = "/etc/grub.d/09_templaros"
111: # The menu entry id is the bare deployment id, which is exactly what hostd passes to
112: # `grub-reboot` and `grub-set-default`.
113: menuentry_id_template = "{id}"
114: # Set by the provisioner in /etc/default/grub. hostd assumes it and never writes that file.
115: default = "saved"
116: # hostd keeps the multi-try counter itself: grub-reboot only affects the next boot and nothing
117: # in GRUB counts attempts.
118: tries_counter = "hostd"
119: 
120: # Dev/VM images built by mkosi keep systemd-boot with Secure Boot off (ADR-0006, ADR-0004).
```

**`TemplarOS/installer/provision.sh:1216–1238`**

```text
1216:         printf '# GENERATED by installer/provision.sh. Identity is by UUID, never by label:\n'
1217:         printf '# every TemplarOS root carries the same label, and a machine with two of them\n'
1218:         printf '# attached would mount whichever the kernel saw first.\n'
1219:         printf 'UUID=%s  /  %s  %s,subvol=@  0 1\n' "$root_uuid" "$ROOT_FORMAT" "$(layout_value root mount_options)"
1220:         for subvol in $SUBVOLUMES; do
1221:             case "$subvol" in
1222:                 /@) continue ;;
1223:                 /@home) dir=/home ;;
1224:                 /@var) dir=/var ;;
1225:                 /@snapshots) dir=/.snapshots ;;
1226:                 /@deployments) dir=/.deployments ;;
1227:                 *) dir="/${subvol#/@}" ;;
1228:             esac
1229:             printf 'UUID=%s  %s  %s  %s,subvol=%s  0 0\n' \
1230:                 "$root_uuid" "$dir" "$ROOT_FORMAT" "$(layout_value root mount_options)" "${subvol#/}"
1231:         done
1232:         printf 'PARTUUID=%s  %s  vfat  umask=0077  0 2\n' "$esp_partuuid" "$ESP_MOUNT"
1233:         [ -n "$data_uuid" ] && printf 'UUID=%s  %s  %s  %s,nofail  0 2\n' \
1234:             "$data_uuid" "$DATA_MOUNT" "$data_fs_type" "${DATA_MOUNT_OPTIONS:-defaults}"
1235:     } > "$mnt/etc/fstab"
1236: fi
1237: note "writing $mnt/etc/templaros/install.toml — what templar-hostd reads to build a boot entry"
1238: if [ "$apply" = 1 ]; then
```

<a id="e17"></a>
## E17 — Newer Home planning explicitly keeps folder-clustered work cards

**`TemplarOS/docs/generated/ACTIVE-WORK.md:82–101`**

```text
82: ### Home's active work leaves the bottom row: window cards sit on free field cells around the centre, clustered by folder (garden link, else recent history), and never move when parked
83: 
84: `active` · `@legeos.work.home-work-centred-folder-clusters-20261004/2` · part of `@legeos.milestone.developer-edition-0-1/1`
85: 
86: User request (2026-10-04, from Lubuntu): the active-window cards on Homegarden (terminals, folders and images together; click to open; a minimized window minimizes to its card) must not be a dedicated bottom row, but placed by a logic that starts them around the centre of the screen and organizes them by folder location when more than one is active, or from recent history. Built: the paginated bottom rail (work_regions, paging, PageUp/PageDown, header) is gone and the persistent grid gets the whole area. shell/src/work_layout.rs (pure) places each window's card on grid cells free of persistent tiles (3x2, 2x1 only when nothing larger fits): windows sharing a folder form a cluster, unfiled terminal/folder/image windows another, everything else a third; clusters ordered by their most recent member start at the grid centre, members gather around their cluster's centroid, other clusters keep a one-cell gap where possible. Existing slots are kept through activation, parking and restoring, so a parked window's card stays where it was; only grid or zoom changes re-arrange everything, and a persistent tile dropped on a card moves only that card. A window's folder is its garden Place association, else the folder its application last touched in Files.Recents1 (DrawerFeed.folders: the entry for a directory mime, else its parent); titles are never read and place stays garden-only. WorkKind::Image covers Graphics apps with Viewer/Photography/RasterGraphics. Windows that fit nowhere are counted on one '+N windows / Show all' card that opens the window overview (window.show-all).
87: 
88: **Acceptance** — 2 of 6 satisfied
89: 
90: | Check | Method | Verdict |
91: | --- | --- | --- |
92: | `headless-smoke` | observation | **satisfied** by `@legeos.evidence.home-work-centred-clusters-render-20261004/1` |
93: | `session-cards-centre-and-cluster` | manual | not satisfied — no evidence |
94: | `session-crowded-field` | manual | not satisfied — no evidence |
95: | `session-park-stays-put` | manual | not satisfied — no evidence |
96: | `session-recent-folder` | manual | not satisfied — no evidence |
97: | `shell-tests` | command | **satisfied** by `@legeos.evidence.home-work-centred-clusters-tests-20261004/1` |
98: 
99: ### Lane C — blooms: an earned Place says what arrived in it, and forgets when you open it
100: 
101: `active` · `@legeos.work.homegarden-blooms-20260909/1` · part of `@legeos.milestone.developer-edition-0-1/1`
```

<a id="e18"></a>
## E18 — The portal work record contains a stale nonexistent backend OpenURI instruction

**`TemplarOS/docs/generated/ACTIVE-WORK.md:68–80`**

```text
68: ### X1 — a usable FileChooser, an OpenURI portal, the portal in the nested session, and an auditable daily-driver capability matrix
69: 
70: `active` · `@legeos.work.developer-edition-daily-driver/1` · part of `@legeos.milestone.developer-edition-0-1/1`
71: 
72: In the mid-plan's priority order: make the native FileChooser usable for browser upload and download (full scrolling listing, hidden toggle, request filters, save-as filename, start folder from Recents1); add org.freedesktop.impl.portal.OpenURI resolving through Associations1 and claim it in legeos.portal; document Flatpak portal selection; audit clipboard, media, suspend, keys, removable storage, secrets and MIME into docs/planning/daily-driver-matrix.md with automated/manual/blocked per row; message package needs to the package-selection lane and manual cases to the hardware lane; start the portal in both nested-session harnesses so the user can run a journey.
73: 
74: **Acceptance** — 1 of 3 satisfied
75: 
76: | Check | Method | Verdict |
77: | --- | --- | --- |
78: | `capability-matrix` | observation | **satisfied** by `@legeos.evidence.daily-driver-matrix-classified-20260907/1` |
79: | `portal-model-tests` | command | not satisfied — `@legeos.evidence.de01-integrated-gate-20260904/1` predates the last change |
80: | `user-journey` | manual | not satisfied — no evidence |
```

<a id="e19"></a>
## E19 — Session activation environment and helper launch ownership already have deliberate handling

**`TemplarOS/session/templar-session/src/main.rs:20–65`**

```text
20: }
21: 
22: const START_STEPS: &[Step] = &[
23:     // A user manager can survive logout.  Remove the previous desktop's socket names before
24:     // importing this login's non-display environment, otherwise templar-comp may select its nested
25:     // backend and try to connect to a compositor that no longer exists.
26:     Step {
27:         program: "systemctl",
28:         args: &["--user", "unset-environment", "DISPLAY", "WAYLAND_DISPLAY"],
29:     },
30:     // greetd's live [initial_session] launches this binary without the wayland-session
31:     // desktop file, so XDG_CURRENT_DESKTOP is unset and `import-environment` would
32:     // ignore it. Set the session identity here; import only the seat/VT greetd does own.
33:     Step {
34:         program: "systemctl",
35:         args: &[
36:             "--user",
37:             "set-environment",
38:             "XDG_CURRENT_DESKTOP=TemplarOS",
39:             "XDG_SESSION_DESKTOP=templaros",
40:             "XDG_SESSION_TYPE=wayland",
41:             "WAYLAND_DISPLAY=wayland-0",
42:         ],
43:     },
44:     Step {
45:         program: "systemctl",
46:         args: &["--user", "import-environment", "XDG_SEAT", "XDG_VTNR"],
47:     },
48:     // Autostart is arbitrated by sessiond (one provider per role); mask the generic runner.
49:     Step {
50:         program: "systemctl",
51:         args: &[
52:             "--user",
53:             "mask",
54:             "--runtime",
55:             "xdg-desktop-autostart.target",
56:         ],
57:     },
58:     Step {
59:         program: "systemctl",
60:         args: &["--user", "start", TARGET],
61:     },
62: ];
63: 
64: const WAIT_STEP: Step = Step {
65:     program: "systemctl",
```

**`TemplarOS/session/templar-session/src/main.rs:107–145`**

```text
107:             .chain(desktop_helpers())
108:             .chain([WAIT_STEP])
109:             .chain(CLEANUP_STEPS.iter().copied())
110:         {
111:             tracing::info!(program = step.program, args = ?step.args, "session step");
112:         }
113:         return Ok(());
114:     }
115: 
116:     let mut helpers = Vec::new();
117:     let result = run_steps(START_STEPS).and_then(|()| {
118:         // Polkit's agent registers a UnixSession subject from its PID. A user-manager service
119:         // has no PAM session, so these children must remain under this greetd-launched process.
120:         // The target start has already waited for the compositor's Type=notify readiness.
121:         for step in desktop_helpers() {
122:             match Command::new(step.program)
123:                 .args(step.args)
124:                 .env_remove("DISPLAY")
125:                 .env("WAYLAND_DISPLAY", WAYLAND_DISPLAY)
126:                 .env("QT_QPA_PLATFORM", "wayland")
127:                 .spawn()
128:             {
129:                 Ok(child) => helpers.push(child),
130:                 Err(error) => tracing::error!(%error, program = step.program, "desktop helper could not start"),
131:             }
132:         }
133:         run_step(WAIT_STEP)
134:     });
135:     stop_helpers(&mut helpers);
136:     // Cleanup also runs after a failed start.  That is what returns SDDM to a usable greeter when
137:     // the compositor cannot take the seat, rather than stranding the user on an empty VT.
138:     for step in CLEANUP_STEPS {
139:         if let Err(error) = run_step(*step) {
140:             tracing::warn!(%error, program = step.program, args = ?step.args, "session cleanup failed");
141:         }
142:     }
143:     tracing::info!(unit = COMPOSITOR_UNIT, "session ended");
144:     result
145: }
```

<a id="e20"></a>
## E20 — The build source closure is not complete merely because PixelKit is attached

**`TemplarOS/Cargo.toml:45–51`**

```text
45: [workspace.package]
46: version = "0.1.0"
47: edition = "2024"
48: # The preview/codecs graph requires Rust 1.97, so the workspace shares that honest floor
49: # instead of advertising an archive baseline its resolved dependencies cannot build (ADR-0008).
50: rust-version = "1.97"
51: license = "AGPL-3.0-only"
```

**`TemplarOS/Cargo.toml:75–101`**

```text
75: templar = { path = "cli/templar" }
76: 
77: # external
78: anyhow = "1"
79: async-trait = "0.1"
80: bpg-decode = { package = "still265", path = "../bpg-rs/crates/still265", features = ["rayon"] }
81: clap = { version = "4", features = ["derive"] }
82: futures-util = { version = "0.3", default-features = false, features = ["std"] }
83: image = { version = "0.25.10", default-features = false, features = ["bmp", "dds", "exr", "ff", "gif", "hdr", "ico", "jpeg", "png", "pnm", "qoi", "tga", "tiff", "webp"] }
84: libc = "0.2"
85: parking_lot = "0.12"
86: # Dev-tool only (`cargo xtask check-interface-signatures`): no service links it.
87: quick-xml = "0.41"
88: # The shell's toolkit and the compositor's own chrome renderer, from the user's pixelkit
89: # workspace. `pixelkit-raster` and `pixelkit-text` have no dependencies of their own, which is
90: # what makes them safe to link into the compositor's render thread; only `pixelkit-shell` touches
91: # winit, so the compositor never sees it.
92: #
93: # Path dependencies because pixelkit has no git remote yet. Before this is packaged, they must
94: # become git dependencies pinned by revision (ADR-0009's vendored source package cannot reach a
95: # sibling directory outside the repository) — a one-line change per manifest, tracked as a
96: # release blocker rather than a design question.
97: pixelkit-raster = { path = "../pixelkit/crates/pixelkit-raster" }
98: pixelkit-text = { path = "../pixelkit/crates/pixelkit-text" }
99: pixelkit-ui = { path = "../pixelkit/crates/pixelkit-ui" }
100: pixelkit-shell = { path = "../pixelkit/crates/pixelkit-shell" }
101: pixelkit-windowing = { path = "../pixelkit/crates/pixelkit-windowing" }
```

**`PixelKit/__AGENT_PACK_REPORT.md:21–41`**

```text
21: 
22: The pack keeps original relative paths and file extensions. It includes recognized source-code, build/configuration, schema, shader, script, documentation, lock, test, and example files. `.akr`, `.claude`, and `.cargo` are the only hidden directories admitted and receive the highest retention priority. Other hidden directories, `target`, `build`, common generated-output/dependency directories, symlinks, binary build artifacts, archives/media/models, likely secret files, and files over the per-file cap are excluded.
23: 
24: ## Scan exclusions
25: 
26: - Files visited: 82
27: - Oversized files: 0
28: - Build/binary artifacts: 5
29: - Possible secrets/private keys: 0
30: - Unsupported/non-development files: 0
31: - Binary-looking files with otherwise text-like extensions: 0
32: - Symlinks: 0
33: - Filesystem errors: 0
34: 
35: ### Artifact examples
36: 
37: - `.akr/cache/index.sqlite` — build artifact or non-agent-readable binary asset
38: - `Archive.zip` — build artifact or non-agent-readable binary asset
39: - `crates/pixelkit-text/test-fonts/NotoSans-Regular.ttf` — build artifact or non-agent-readable binary asset
40: - `crates/pixelkit-text/test-fonts/NotoSansThai-Regular.ttf` — build artifact or non-agent-readable binary asset
41: - `pixelkit-agent-pack-2026-09-12_170654.zip` — build artifact or non-agent-readable binary asset
```

<a id="e21"></a>
## E21 — MIME associations already have an atomic compatibility export

**`TemplarOS/crates/templar-associationd/src/dbus.rs:43–71`**

```text
43:     pub fn new(
44:         engine: Arc<RwLock<AssociationEngine>>,
45:         detector: Arc<dyn MimeDetector>,
46:         store: Store,
47:         catalog: Arc<dyn AppCatalog>,
48:         mimeapps_path: PathBuf,
49:     ) -> Self {
50:         Self {
51:             engine,
52:             detector,
53:             store,
54:             catalog,
55:             mimeapps_path,
56:         }
57:     }
58: 
59:     /// Best-effort: a failure to regenerate `mimeapps.list` (e.g. a read-only home during
60:     /// recovery) must not fail the association change itself, which is already durably
61:     /// persisted in `templar-state`.
62:     fn regenerate_mimeapps(&self) {
63:         let engine = self.engine.read();
64:         if let Err(error) =
65:             export::export_mimeapps(engine.database(), &*self.catalog, &self.mimeapps_path)
66:         {
67:             tracing::warn!(%error, path = %self.mimeapps_path.display(), "failed to regenerate mimeapps.list");
68:         }
69:     }
70: }
71: 
```

**`TemplarOS/crates/templar-associationd/src/export.rs:15–51`**

```text
15: 
16: /// Render the `mimeapps.list` text for `db`.
17: pub fn render_mimeapps(db: &AssociationDb, catalog: &dyn AppCatalog) -> Result<String> {
18:     let mut output = String::new();
19:     output.push_str(
20:         "# Generated by TemplarOS associationd. Use System Settings or `templar open` to edit.\n\n",
21:     );
22:     output.push_str("[Default Applications]\n");
23: 
24:     for (mime, app_id) in &db.mime_defaults {
25:         let manifest = catalog
26:             .manifest(app_id)
27:             .with_context(|| format!("missing manifest for {app_id}"))?;
28:         validate_desktop_id(&manifest.primary_desktop_file_id)?;
29:         writeln!(output, "{mime}={};", manifest.primary_desktop_file_id)?;
30:     }
31: 
32:     output.push_str("\n[Added Associations]\n");
33:     // Adding the same desktop entry here ensures each selected default is also registered as
34:     // supporting the MIME type, as the mime-apps spec requires.
35:     for (mime, app_id) in &db.mime_defaults {
36:         let manifest = catalog
37:             .manifest(app_id)
38:             .with_context(|| format!("missing manifest for {app_id}"))?;
39:         writeln!(output, "{mime}={};", manifest.primary_desktop_file_id)?;
40:     }
41: 
42:     Ok(output)
43: }
44: 
45: /// Render and atomically write `mimeapps.list` to `destination`.
46: pub fn export_mimeapps(
47:     db: &AssociationDb,
48:     catalog: &dyn AppCatalog,
49:     destination: &Path,
50: ) -> Result<()> {
51:     let output = render_mimeapps(db, catalog)?;
```

<a id="e22"></a>
## E22 — Preserve newer UUID-aware and deployment-specific boot entry handling

**`TemplarOS/services/templar-hostd/src/boot_entries.rs:60–91`**

```text
60: pub enum RootSpec {
61:     Uuid(String),
62:     Label(String),
63: }
64: 
65: impl RootSpec {
66:     /// Read `root_uuid` from `<root>/etc/templaros/install.toml`, falling back to the label.
67:     #[must_use]
68:     pub fn read(root: &Path) -> Self {
69:         std::fs::read_to_string(root.join(INSTALL_TOML))
70:             .ok()
71:             .as_deref()
72:             .and_then(Self::parse)
73:             .unwrap_or_else(|| Self::Label(ROOT_FS_LABEL.to_owned()))
74:     }
75: 
76:     /// `root_uuid = "…"` out of the file. A deliberately small reader: the file is flat, and the
77:     /// boot path is the last place to add a dependency that can be missing on a rescue shell.
78:     #[must_use]
79:     fn parse(text: &str) -> Option<Self> {
80:         text.lines()
81:             .map(str::trim)
82:             .filter(|line| !line.starts_with('#'))
83:             .find_map(|line| line.strip_prefix("root_uuid"))
84:             .and_then(|rest| rest.trim_start().strip_prefix('='))
85:             .map(|value| value.trim().trim_matches(['"', '\'']).to_owned())
86:             .filter(|uuid| !uuid.is_empty())
87:             .map(Self::Uuid)
88:     }
89: 
90:     /// The `root=` word for a kernel command line.
91:     #[must_use]
```

**`TemplarOS/services/templar-hostd/src/boot_entries.rs:173–194`**

```text
173:          # rollback; GRUB_DEFAULT=saved selects among these by deployment id.\n",
174:     );
175:     for entry in entries {
176:         let id = &entry.id;
177:         // `search --file` rather than a filesystem label: it finds the ESP by something that is
178:         // true of it in every image we produce, including the mkosi one, whose repart
179:         // configuration sets no filesystem label at all.
180:         let _ = write!(
181:             out,
182:             "\nmenuentry '{title}' --id '{id}' {{\n    \
183:              search --no-floppy --set=root --file {dir}/{id}/vmlinuz\n    \
184:              linux {dir}/{id}/vmlinuz {root} rootflags=subvol={subvolume} rw {key}{id}\n    \
185:              initrd {dir}/{id}/initrd.img\n",
186:             title = entry.title,
187:             dir = ESP_DEPLOYMENT_DIR,
188:             root = root.cmdline_arg(),
189:             subvolume = entry.subvolume,
190:             key = CMDLINE_KEY,
191:         );
192:         if let Some(modules) = &entry.nvidia_modules {
193:             let _ = writeln!(out, "    # nvidia-modules {modules}");
194:         }
```

<a id="e23"></a>
## E23 — Display and input safety already have active acceptance criteria

**`TemplarOS/docs/generated/ACTIVE-WORK.md:1004–1030`**

```text
1004: ### Graphical Settings for displays, mouse/trackpad and Home appearance
1005: 
1006: `active` · `@legeos.work.home-field-settings-gui/1` · part of `@legeos.work.home-surface-usability-next/1`
1007: 
1008: The graphical Settings app already edits the five Home Field tuning keys; replace the vague goal of making every read-only diagnostic tab editable with the common controls needed for daily use.
1009: 
1010: Displays: enumerate connected outputs and allow resolution, refresh rate, rotation/orientation and scale per output, including mixed-scale arrangements. Apply through compositor-owned validated configuration with a visible confirmation countdown and automatic rollback when the new mode is not confirmed or the output disappears. Show the resulting logical arrangement before applying and retain a known-good configuration.
1011: 
1012: Mouse and trackpad: expose pointer speed, acceleration profile, natural scrolling, primary-button choice and scroll behaviour; for touchpads also tap-to-click, tap-and-drag, two-finger/edge scrolling and disable-while-typing where libinput supports them. Unsupported controls are identified as unavailable rather than drawn as inert switches.
1013: 
1014: Appearance: select light/dark or bundled themes, accent and object-material palette/texture roles, background/wallpaper behaviour, and the defaults used by Home panels, folder panes and widgets. Themes are declarative design tokens/material assets, never executable shell extensions, and application fullscreen remains free of Home chrome. Every graphical setting writes through a typed service/schema and has Reset; CLI inspection/change remains equivalent. Provider, association, history and hardware diagnostic pages may remain read-only until their own scoped work exists.
1015: 
1016: **Acceptance** — 2 of 5 satisfied
1017: 
1018: | Check | Method | Verdict |
1019: | --- | --- | --- |
1020: | `mixed-scale-preview` | command | not satisfied — no evidence |
1021: | `mouse-trackpad-controls` | command | not satisfied — no evidence |
1022: | `safe-display-configuration` | command | not satisfied — no evidence |
1023: | `theme-and-material-tokens` | command | **satisfied** by `@legeos.evidence.home-chips-projects-appearance-lock-20260907/1` |
1024: | `typed-service-parity` | command | **satisfied** by `@legeos.evidence.home-chips-projects-appearance-lock-20260907/1` |
1025: 
1026: ### Window groups — one unit of work: grouped windows park, return, stack and drag together, placed beside and outlined; Super+Tab across groups
1027: 
1028: `active` · `@legeos.work.window-groups/3` · part of `@legeos.work.home-surface-usability-next/1`
1029: 
1030: Implement @legeos.decision.window-groups-are-explicit-and-compositor-owned in four steps then a hands-on: WG1 the model in lege-window (GroupId, membership on WindowRecord, group/ungroup/dissolve, raise_many and group-contiguous activate, shelve/unshelve/move_group, place_beside, group_outline, accent palette, cycle_within_group and cycle_groups, group on ShellToplevel, new WindowCommands, property tests; the Super+Left/Right step renamed window.step-right/left); WG2 the compositor (Alt+Tab within group, Super+Tab across groups, Super+G group-with-previous placing beside, Super+Shift+G ungroup, HUDs, band accent stripe and name, group outline as per-window chrome, band-on-band drop groups and places beside with a drop highlight, band drag moves the group and Shift+drag one member, COMPOSITOR.md rows); WG3 lege_shell_v1 toplevel v3 / manager v5 (both ends at SHELL_PROTOCOL_VERSION 5; group event into ShellToplevel.group; join/leave/activate/shelve/unshelve/name requests); WG4 the Home tile (TileKind::WindowGroup container of ParkedSession tiles, merge_sessions placement and sweep, drop-to-group and drag-out gestures, container drag as one, title press activates the group, Park shelves the group, ladder rungs, accent edge, visual baseline); WG5 the user's hands-on acceptance.
```

<a id="e24"></a>
## E24 — Existing seeds include PCManFM-Qt, GTK portal and Qt Wayland support

**`TemplarOS/distro/seeds/selection.toml:326–341`**

```text
326: name = "libpam-fprintd"
327: reason = "fingerprint-first greetd PAM authentication, with the normal login password stack as fallback"
328: 
329: [[layer.package]]
330: name = "xdg-desktop-portal"
331: reason = "the portal frontend every sandboxed app talks to; backends are chosen behind it"
332: 
333: [[layer.package]]
334: name = "xdg-desktop-portal-gtk"
335: reason = "Print, Email, Notification and DynamicLauncher, which the TemplarOS backend does not implement and which otherwise fail outright for Flatpak apps"
336: 
337: [[layer.package]]
338: name = "xwayland"
339: reason = "X11 clients still exist and a developer machine meets them weekly"
340: 
341: [[layer.package]]
```

**`TemplarOS/distro/seeds/selection.toml:366–383`**

```text
366: 
367: [[layer.package]]
368: name = "templaros-settings"
369: reason = "the settings surface for the policy templaros-sessiond owns"
370: 
371: [[layer.package]]
372: name = "pcmanfm-qt"
373: reason = "the FileManager provider role's default (templar-session-core/src/providers.rs's DEFAULT_FILE_MANAGER); TemplarOS's own Files is too rudimentary to be the session's file manager, so the composition ships a complete one and integrates it through filed (docs/architecture/files.md)"
374: role = "file-manager"
375: 
376: [[layer.package]]
377: name = "qt6-wayland"
378: reason = "the Qt 6 Wayland platform plugin pcmanfm-qt needs; without it a Qt application in this session falls back to xcb or refuses to start"
379: 
380: [[layer.package]]
381: name = "kitty"
382: reason = "the Terminal provider role's default (templar-session-core/src/providers.rs:157) and the terminal templaros-shell ships /etc/xdg/kitty/kitty.conf for"
383: role = "terminal"
```

<a id="e25"></a>
## E25 — PixelKit already has a reusable theme data structure

**`PixelKit/crates/pixelkit-ui/src/widget.rs:37–86`**

```text
37: use pixelkit_text::{Align, FaceId, TextCache, TextStyle};
38: 
39: /// Colours and sizes, in one place so a screen does not invent its own.
40: #[derive(Debug, Clone, Copy)]
41: pub struct Theme {
42:     pub background: u32,
43:     pub panel: u32,
44:     pub panel_alt: u32,
45:     pub panel_edge: u32,
46:     pub text: u32,
47:     pub text_dim: u32,
48:     /// For a figure that is bad news — short, negative, unexplained.
49:     pub alarm: u32,
50:     pub alarm_soft: u32,
51:     pub good: u32,
52:     pub good_soft: u32,
53:     pub accent: u32,
54:     pub accent_soft: u32,
55:     /// A row under the cursor.
56:     pub hover: u32,
57:     /// The selected row.
58:     pub selection: u32,
59:     pub row_height: i32,
60:     /// Body text.
61:     pub body: TextStyle,
62:     /// Headings.
63:     pub heading: TextStyle,
64:     pub padding: i32,
65:     pub corner_radius: i32,
66:     pub border_width: i32,
67: }
68: 
69: impl Default for Theme {
70:     fn default() -> Theme {
71:         Theme {
72:             // Dark, because a back office is often a corner of a kitchen with
73:             // a window behind the screen.
74:             background: 0x0016_1a1f,
75:             panel: 0x001e_242b,
76:             panel_alt: 0x001a_2026,
77:             panel_edge: 0x002c_343d,
78:             text: 0x00e8_edf2,
79:             text_dim: 0x0090_9aa5,
80:             alarm: 0x00e0_6c5f,
81:             alarm_soft: 0x003b_2928,
82:             good: 0x006f_c28a,
83:             good_soft: 0x0024_3a2d,
84:             accent: 0x004d_9fd6,
85:             accent_soft: 0x0021_3847,
86:             hover: 0x0027_2f38,
```

## Search-scope observations

The inspected compositor and interface source did not identify an implemented wlr-output-management control endpoint or an equivalent complete configuration API. `OutputManagerState::new_with_xdg_output` is output advertisement, not evidence of that control endpoint. Verify against the implementation checkout before adding an API.

Searching the supplied Rust source for `portals.conf`, `default=templaros`, and the claimed generated portal configuration found configuration checks but did not establish the regeneration promised by the shipped configuration comment. Treat regeneration as unverified, not implemented by assertion.

Searching session, shell and compositor source for `dim_after`, `lock_after`, `suspend_after`, and `lid_closed` did not identify the remaining policy enforcement beyond existing screen-off handling. Manual locking is separately implemented and must not be described as still absent.

## Display alpha integration input

The previous alpha contains `src/model.rs`, `src/policy.rs`, `src/service.rs`, `src/ipc.rs`, `src/backend/{x11,wlr}.rs` and `src/ui.rs`. Its Cargo manifest names an optional eframe 0.31.1 GUI. Reuse candidates in this plan are architectural suggestions, not a claim that the alpha builds or passes tests. Its prior README explicitly recorded that compilation and physical display validation were unavailable. The Templar integration must not start its independent helper alongside the compositor's new display authority.
