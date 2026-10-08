# Homegarden previews, notification popups, and corner-following window tabs

Status: **implemented and under review; acceptance remains proposed**. Requested
2026-10-07. The October 7 continuation repaired integration gaps and exercised an
owned nested desktop; owner prototype review and the gates below remain open.
AKR is the planning authority; this document expands:

- `@legeos.work.homegarden-visible-previews-and-tab-counts-20261007`
- `@legeos.work.visible-notifications-pixelkit-20261007`
- `@legeos.work.external-window-tabs-20261002` and its four existing children,
  revised to the October 7 interaction rather than leaving a competing plan.
- Findings: `@legeos.assessment.desktop-preview-notification-tab-scope-20261007`.
- Runtime evidence: `@legeos.evidence.notification-delivery-scope-20261007`.

The source tables below preserve the initial scoping baseline over working tree
`7c68786687bea7b20dbc382bdacc2804e7284b70`, not the final implementation state.
Current review observations are recorded in the continuation section. All runtime
work used an owned compositor, private bus, temporary HOME/XDG directories and
native binaries. No installed service, live desktop, deploy or logout was changed.

## 1. Homegarden: current-work previews and application tab counts

### Initial scoping baseline

| Area | Observed source | Consequence |
|---|---|---|
| Window capture | `compositor/templar-comp/src/screencopy.rs`: trusted-shell-only foreign-toplevel/image-capture protocols; surface-tree render, GPU readback into SHM | Reuse this, not file-thumbnail codecs or a second screen-recording daemon. |
| Bounds | `MAX_PREVIEW_SIDE = 512`, `MIN_FRAME_INTERVAL = 250 ms` | Existing maximum is 4 captures/s/window, not video-rate playback. |
| Shell transport | `shell/templar-shell/src/previews.rs`: per-window sessions and reusable SHM buffers; `ready` reads pixels into a bitmap | Pixel ownership/cancellation spans both processes. Hiding only the card does not cancel queued capture work. |
| Current defaults | `shell/templar-shell/src/settings.rs`: `previews = Static`, live rate 1, configured rate clamped to 1–4 | A still preview is not evidence that capture is absent. |
| Scheduling | `surface/app.rs::refresh_previews` walks toplevels; `previews_covered` only checks an unrevealed Home plus a maximized/fullscreen window | It does not exclude every obscured/offscreen/overflow card. Ordinary windows can cover cards while capture still runs. |
| Painting | `surface/field_screen.rs::occlude_windows` erases Home content under windows after drawing | Draw-then-erase does not meet “not rendered when not visible.” |
| Other consumers | `surface/app.rs::refresh_fan_previews`, bar redraw and shell Alt-Tab rendering share the bitmap cache | Home-only is a deliberate behavior change: remove preview requests **and preview painting** from these consumers, retain their icons, titles and window actions. |
| Window metadata | `crates/templar-window/src/shell.rs::ShellToplevel` has window/app identity, title, geometry and state, not application tab counts | Number of windows, processes, panes or window-group members cannot substitute for tabs. |

### Proposed visible-only contract

Show the **currently selected content of each application window**, including open
and parked windows, in its existing Homegarden work card. Preserve card positions,
folder clusters, activation/restore, MRU, recents, bar switching and Alt-Tab behavior.
A preview is not interactive remote control and does not expose every background tab.

1. The shell derives capture demand from the actual Home card layout and visible
   clipping/occlusion, per output: only wells with visible pixels are eligible.
   Home reveal is sufficient but not necessary: a card exposed on the ordinary
   desktop is visible too. Offscreen, overflow-only, zoom-hidden, fully occluded,
   locked, output-disabled and absent-shell cards demand nothing.
2. Send changed demand to the compositor on the authenticated shell connection.
   Reuse existing capture transport; add only the visibility/demand information
   it lacks. A trusted-shell request is still not proof that its card is visible.
   Version the interface and validate window IDs against the shell's live session.
3. Check demand both at capture admission and immediately before offscreen render.
   Retire queued frames without rendering after demand disappears, using the
   protocol's failure/release lifecycle. A frame already executing when visibility
   changes may finish; its stale result must not repaint, schedule another capture,
   or retain an unnecessary pool. Define the visibility-generation boundary.
4. Live mode is the proposed default, 1 fps, adjustable within the existing 1–4 fps
   cap. Retain off/static options. Use damage/commit-driven invalidation with a
   visible-only rate deadline: no recapture of unchanged pixels, no idle polling,
   and no chain driven indefinitely by capture-ready events. First reveal needs a
   redraw even if the underlying window/output is otherwise idle.
5. Keep at most one in-flight capture per demanded window and bound aggregate
   memory/work as well as per-window rate. Reuse render targets and readback/SHM
   storage where supported rather than allocate a fresh renderbuffer per frame.
   Initial proposal: 16 MiB aggregate retained thumbnail/pool budget, fair service
   to visible cards, no background captures to warm a cache. A 512×512 RGBA bitmap
   alone costs 1 MiB; count every CPU/GPU/pool copy, not just the shell bitmap.
6. On ordinary hide, an already-built last frame may remain under that budget but
   is not drawn or refreshed. Release inactive capture resources; purge pixels on
   close, identity loss, lock/privacy invalidation or memory pressure. On reveal,
   show a retained last frame until a fresh one arrives, never claim it is current
   before capture succeeds. Missing/denied capture shows icon/title, not a black
   image presented as success.
7. This stops **thumbnail** rendering, not the application's own job or normal
   rendering while its real window is visible. Parked clients may throttle or stop
   submitting buffers. Live-preview demand may require bounded compositor frame
   callbacks for that source; it must not advertise the real window as foreground
   or visible to resource policy just to animate its thumbnail. If the client has
   not submitted a new buffer, the last submitted content is all we can show.

### Application coverage and limits

- Terminals and browsers: same compositor surface-capture route; no OCR, terminal
  text scraping, browser debugging port, or app-specific capture backend needed.
- VLC/video: ordinary submitted video surfaces should use that route too
  **[INFERENCE; not exercised here]**. Check GPU buffers, subsurfaces, hardware
  decode, direct-scanout transitions and parked-client throttling in real runs.
  A 1–4 fps preview is activity feedback, not a second media player.
- Photoshop: if it is a local capturable Wine/XWayland or other guest window, the
  compositor can attempt that window's pixels **[INFERENCE]**. This is not a promise
  of native Linux Photoshop support, per-window capture inside an opaque VM
  desktop, or capture of protected content. Validate the user's actual setup when
  available; do not block terminal/browser support on it.
- Never bypass protected-content/capture refusal. Do not persist preview images.

### Tab counts: separate metadata integration

Display a small numeric count on each Home work card, and make the same metadata
available to the window's horizontal title tab. Count application tabs belonging
to **that OS window**, including inactive tabs. A terminal pane/split is not another
tab; a browser tab group is not a single tab. A verified count of one may show `1`;
unknown or disconnected metadata shows no fabricated `0`/`1` badge.

No generic compositor window protocol in the inspected path supplies this count.
“Any terminal/browser” is therefore an interoperability goal, not a capability we
can obtain by counting windows. Track supported applications explicitly:

| Producer | Proposed route | Prerequisite / risk |
|---|---|---|
| Kitty | Its documented `kitten @ ls` tree has OS windows → tabs → panes; use an in-process event integration or narrowly authorized local metadata adapter | Prove its OS-window ID maps to the exact compositor toplevel, especially multiple Wayland windows in one process. Never enable unrestricted remote control merely for a badge. |
| Firefox/Chromium-family | Extension reads tabs by browser `windowId`; native messaging carries counts and tab create/remove/attach/detach transitions | Browser window IDs are not Wayland surface IDs. A brokered per-window binding/native integration is required; PID/app ID/title matching is insufficient for multiple windows with identical titles. |
| Other terminals/apps | Their own tab-change API, an upstream metadata integration, or an application-specific adapter | No honest universal fallback. Record unsupported as unknown; do not scrape titles or infer from process trees. |

Proposed shared value: a count plus connection/window generation and provenance,
associated with a compositor-issued window identity. Sessiond owns adapter
attribution/lifecycle; compositor owns the surface identity; shell owns the badge.
Use sender credentials and an explicit per-window registration/binding mechanism.
A count-only claim must not authorize focus, close or capture. Strip URLs, tab
names, terminal text and command lines from the metadata sent to the shell.
On adapter restart or window destruction invalidate the old count; tab changes
are pushed, not polled while Home is hidden. Tab metadata may update while hidden;
that is not preview rendering.

**Unresolved implementation prerequisite:** a reliable native Wayland mapping for
browser/terminal internal window IDs. The implementation must demonstrate this
before claiming multi-window support; an extension alone does not solve it.

### Acceptance

- Real terminal output and changing browser content update the correct Home cards;
  parked windows use fresh submissions when available without being raised.
- Hide Home, fully cover a card with ordinary windows, scroll/zoom it away, lock,
  disable its output and close it with a capture in flight. After the visibility
  boundary, observe zero new thumbnail renders/readbacks/requests for that card;
  unrelated visible cards continue. Merely observing no paint is insufficient.
- Bar rows, Alt-Tab and Taskfield still work but draw no window-pixel previews.
- Run steady content and a 20-window changing workload: bounded memory, fair visible
  updates, no recurring capture wake while hidden, no per-frame resource growth.
- Two same-app windows with equal titles but different tab counts remain correctly
  bound through tab detach/reattach, window close, park/restore and adapter restart.
  Unknown metadata never becomes a made-up count; splits do not inflate tabs.
- Exercise native Wayland and XWayland separately; report VLC/Photoshop/protected
  content results separately, not as proven by a terminal screenshot.

## 2. Notifications: visible bottom-right, styled with PixelKit

### What exists and why delivery needs attention

`services/templar-sessiond/src/apis/notifications.rs` already owns freedesktop
notifications, searchable history, active rows, replacement IDs, expiry and actions.
`background/notification_provider.rs` acquires the standard bus name only while
sessiond is the selected provider. Keep that authority and provider selection.

The shell has a notification Home widget **and** a separate toast window:

- `surface/app.rs::spawn_notifications_watch` subscribes to
  `org.templaros.Notifications1.Changed` and then fetches active rows.
- Source and rust-analyzer references show only dismiss, clear and invoke emit
  that signal. `Notify`, replacement, expiry and `CloseNotification` do not.
- The current toast is 380×92, newest-ID-only, top-right via
  `templar-window::shell_surface_placement`, and expires after six seconds.
- The shell projection drops body, category, urgency, timestamps and icon. The
  highest-ID watermark misses same-ID replacements and collapses burst arrivals.
  Initial active rows intentionally do not toast.
- `session/profile.d/60-templaros-garden.sh` reports Bash CWD changes. It does not
  report command completion. Pixels cannot reveal that a shell job finished.

The user's invisible-notification report is accepted. These source findings are
not a claim that the installed desktop was inspected or that every invisibility
cause has been isolated.

### Runtime observation made during scoping

Ran the existing native `target/native/ubuntu-26.04/debug/templar-sessiond` on an
owned private D-Bus with an empty temporary HOME/XDG/database/catalogue. Both session
and system bus addresses pointed at the fixture bus; no live service name, desktop,
configuration or system bus was touched. Binary SHA-256:
`ada2cfd2f570e3e5563c28b0e426c3285965993716a136cbfdd6b3ef15ea8980`.

- `Notify` returned ID 1; `ListActive` contained the exact summary/body and a
  5000 ms server-default expiry.
- Zero `Notifications1.Changed` signals after arrival; zero after expiry; the
  active list was empty after expiry.
- A second, persistent notification was dismissed through `Notifications1`.
  Dismiss returned true and emitted exactly one `Changed` at
  `/org/templaros/Session/Notifications`, proving the signal monitor worked.
- This was a D-Bus experiment, **not a visual popup test**, and not a fresh build
  of all current sources. Owned processes were stopped/reaped and fixture removed.

### Proposed ownership and behavior

1. **Service:** publish committed creation/replacement/close/expiry/dismiss/action
   transitions. Standard `NotificationClosed`/`ActionInvoked` use the freedesktop
   path/interface; companion changes use the TemplarOS path/interface. Preserve
   replacement-generation guards against an old timer expiring a newer message.
2. **Read model:** expose enough versioned structured data to distinguish same-ID
   replacement from already-presented content. Subscribe before snapshot, reconcile
   service-owner changes, and do not use a maximum numeric ID as an event cursor.
   Reuse existing observation conventions, not a new periodic fetch loop. Hide a
   visible toast immediately when its active notification is closed/replaced.
3. **Presentation:** one shell-owned Notification-layer host, bottom-right of the
   chosen output, above ordinary applications, below secure/recovery/system-modal
   surfaces. Never steal keyboard focus, enter Alt-Tab, or become an application
   work card. Start on the focused window's output (pointer output if none), and
   keep the toast on that output until it leaves; re-clamp on hotplug/scale change.
4. **Layout:** compact themed card with app/icon, summary and optional short body;
   explicit dismiss and declared actions. Initial width about 380 logical px,
   content-sized bounded height. Safe bottom/right inset must account for the
   existing nonexclusive bottom bar, machine cluster, corner controls and gesture
   zones—`Output::usable()` alone cannot see all of those. Do not reserve workspace
   or resize clients. In fullscreen, ordinary toasts defer; critical ones may show
   without focus theft. Lock hides message content and stops toast painting.
5. **Duration:** proposed normal presentation dwell is 3 s for successful terminal
   jobs, 4 s for general informational events, 5 s for failure/actionable events.
   Application/category settings may select within 3–5 s. Use generic 4 s when
   producer type is unknown; caller-supplied app names are not trusted identity.
   Dwell begins on first presentation, not receipt. Hover/keyboard interaction may
   pause dismissal as an accessibility behavior, then resume the remaining time.
6. **Separate clocks:** hiding the toast is not deleting searchable history or
   falsely dismissing the server notification. Preserve freedesktop timeout rules:
   zero means no server expiry, positive is sender-requested lifetime, -1 is server
   default. Earlier server close/expiry cancels the toast; never resurrect an expired
   message just to give it 3–5 s. A persistent/critical notification can retain an
   active/history entry after its popup disappears. Document these exceptions.
7. **Bursts:** bounded presentation queue/stack, not newest-wins loss. Proposal:
   at most three visible cards, twenty waiting; replacement updates its card in
   place. Excess events remain in existing history and produce a compact count,
   not unbounded windows or a minutes-long replay. Stale queued messages do not
   appear after unlock/fullscreen exit or service restart.

### PixelKit extension, not another notification service

`../pixelkit/crates/pixelkit-ui/src/lib.rs` exports widgets, palette, focus,
semantics, layout and tooltips but no toast component. The shell currently paints
its own popup with primitive fills and labels. Extend **pixelkit-ui** with a
host-independent notification/toast card: borrowed presentation data, measured
layout, existing theme tokens, icon/summary/body/actions/dismiss affordances,
shared paint/hit geometry, keyboard focus and accessible announcement semantics.
Reuse the existing scale/text/raster primitives and widget outcome pattern.

The widget returns action/dismiss outcomes. It must not own D-Bus, application
classification, history, timers, output choice, Wayland privileges, or a winit
window. Templar-shell owns the bounded visible stack and its next deadline;
TemplarOS policy remains in sessiond/settings. Reuse PixelKit's existing hosting
adapter; add a reusable host capability only if the required no-focus/input-region
behavior cannot be expressed through it. Do not add a second toolkit event loop.

### Terminal completion producer

A working notification renderer does not by itself notify for arbitrary commands.
Keep this work explicit:

- Applications already sending `org.freedesktop.Notifications.Notify` require no
  extra completion detector.
- Add shell integration for foreground command/pipeline completion: capture start
  time and exit status without changing `$?`, prompt hooks, job control, or existing
  Homegarden CWD reporting. Proposed default: notify when a command took at least
  10 s and its terminal is not focused; explicit opt-in can notify every job.
- Bash is already integrated; zsh/fish need their own event-hook contracts. This
  makes support independent of terminal brand when its shell cooperates, not
  magically available in every shell. Background jobs need their actual completion
  event or an explicit wrapper; a prompt reappearing does not prove a background
  job finished. An explicit `templar run` completion route may reuse its owned
  child exit, but must not duplicate a shell/terminal's own notification.
- Count one completed pipeline/job, distinguish success/failure, suppress canceled
  jobs by default, and deduplicate native terminal notifications. Prefer a safe
  label and elapsed/status information over persisting raw secret-bearing command
  lines. No process polling or inference from screen text.

### Acceptance

- Private-bus real producer → committed notification → shell bottom-right popup,
  including replacement of the same ID, a burst, sender close, expiry, dismiss and
  a declared action reaching the producer exactly once on the standard interface.
- Observe styled pixels on a real owned nested display, no focus/MRU change, no
  accidental overlap with bottom controls, three duration classes from first
  presentation, correct HiDPI/multi-output/hotplug positioning and history retention.
- Long-running foreground terminal success/failure, pipeline status, Ctrl-C,
  background completion, duplicate native notification and unchanged prompt/CWD
  semantics. Report actual Bash/zsh/fish coverage, not “all terminals” from one run.
- No repaint/timer after the last toast retires; no message pixels while locked;
  no stale replay or lost new arrivals across shell/service restart.

## 3. Window-decoration tabs at any corner

### Resolve the wording explicitly

These are compositor **title/control tabs**, not browser/terminal tabs.
The request first says “hidden until mouseover” and ends “tabs always visible but
they move to the closest corner on mouseover.” Proposed interpretation, preserving
the final instruction:

- One horizontal name/title tab remains visible while its window is visible.
- Minimize/park, maximize/restore and close controls reveal on pointer approach;
  an always-visible-controls option remains available.
- Hover regions exist along **all four sides**, selecting the closest of four
  corners; this does not mean four duplicate title bars permanently surround each
  window. After leaving, retain the last selected title corner rather than snap back.
- Default pair forms a right angle at the **same corner**: title on the adjacent
  horizontal edge, controls vertically on the adjacent left/right edge. Title
  text never rotates or occupies a vertical title strip.
- Options can select opposite-corner placement instead. Proposed precise legacy
  arrangement: title and horizontal controls at opposite ends of the same top or
  bottom edge. Keep corner-following independent of that layout choice.

The visibility interpretation and precise opposite-corner layout are proposals to
review in the first interactive prototype, not claims of an already settled UI.
The always-visible title also applies to maximized/fullscreen windows through a
bounded overlay; if fullscreen should hide it, that is a policy change to confirm,
not a silent exception to “always visible.” Lock and genuine occlusion still win.

### Existing source and plan conflict

Today's `geometry.rs::client_area/frame_around/tag_strip` reserves 32 px below the
client. `chrome/layout.rs::name_tag/control_tags` puts name left and horizontal
controls right; narrow strips drop Park/Maximize first. `input.rs::press_target_at`
resolves controls before exterior resize. Recovery is based on the bottom strip.
Changing only the renderer would leave input, client sizing and recovery wrong.

The October 2 proposed decision explicitly prohibited pointer-following anchors,
required one fixed side, and defaulted to hiding all tabs. Its corner safety and
shared-layout requirements remain useful, but those three policies conflict with
this request. Revise that **proposed** decision and its existing work children via
AKR, including obsolete interface-path scopes and acceptance criteria. The old
`...stationary` key remains a historical identifier; its title/body must no longer
assert that pointer following is forbidden.

### Proposed shared geometry and interaction

Use one `WindowLayout` owned by templar-window for client rect, active corner,
name rect, fixed-size control rect array, four edge/corner activation regions,
resize zones, visible input regions, recovery targets and exterior/overlay
placement. Compositor paint, damage, hover/press, move/resize, configure, recovery,
XWayland and group hit routing consume it. Do not build separate coordinate math
in PixelKit or in a shell overlay; the compositor owns decoration interaction.

- Clean cutover from the frame-minus-bottom-strip model. Preserve current client
  dimensions during migration; moving/showing chrome never sends a client resize.
  Remove obsolete helper callsites and covering-chrome special state after their
  behavior is represented by the common layout, not by compatibility shims.
- Select corners from pointer distance to the window's **client** corners, with
  deterministic ties and hysteresis near boundaries. Latch the corner while the
  pointer is on a visible title/control, and throughout a press, move, resize or
  menu interaction. Reconsider only after exiting that target corridor/grab.
  This implements “moves to the closest corner” without making Close flee the mouse.
- Entry/reveal does not activate, raise, or execute a control. Close requires a
  fresh press/release on a visible stationary target. A hidden control takes no
  input. Higher windows and shell overlays retain priority over underlying hover
  bands; gaps outside the client fall through.
- Every corner offers title-drag/move, perimeter resize, park/minimize,
  maximize/restore and close. Reserve non-overlapping resize corner squares;
  controls sit alongside/inset, never on the exact resize target. On tiny windows,
  shorten the title and use bounded placement/overflow for controls rather than
  silently losing the ability to park or maximize.
- Without exterior room, clamp an overlay to the same corner inside the window's
  output area. Respect the bottom gesture/peek zones, notifications, lock and
  higher-priority overlays. Output removal/recovery must leave a usable title and
  control route. An unavailable corner is not permission to place Close offscreen.
- Keyboard-only users retain window menu/close/move/resize shortcuts and a way to
  reveal/focus controls. Reduced motion disables animation, not reachability.
  Pointer-following changes decoration pixels/input regions, never focus/MRU.
- Proposed options (names not shipped APIs): `windows.tabs.placement =
  same-corner | opposite-corners`, `windows.tabs.follow-pointer = true`,
  `windows.tabs.controls = hover | always`, and a default corner for new windows
  (proposed top-right). Title visibility is independent and defaults to always.
  A fixed-corner option provides a predictable accessibility fallback. Remove the
  older proposed follow-resize-only policy rather than let two anchor owners race.

### Revised work sequence and acceptance

1. **Corner safety:** reserve resize geometry around visible client corners and
   prove close/resize cannot share a target, including every restore/repair path.
2. **Shared layout:** client-space geometry; four real corners, both arrangements,
   horizontal titles, vertical/default and horizontal/opposite controls; migrate
   all paint/hit/configure/recovery consumers. No fake Left/Right alias to Top.
3. **Hover/follow state:** always-visible title plus revealed controls, closest
   corner selection, hysteresis/latching, hidden-input exclusion, keyboard path,
   overlay placement, and one damage/deadline authority.
4. **Options and actual interaction:** relay committed settings through the current
   `templar-shell-v2.xml` contract, expose Settings controls, then exercise both
   arrangements on owned native Wayland and XWayland windows.

Gate with actual pointer interactions at all four corners, including rapid
crossing, opposite-direction movement, tiny windows, overlapping windows, group
move, parked restore, maximize/fullscreen restore, scale changes and output loss.
Count client configure messages during pure tab relocation: no geometry change.
Observe that the close target remains under the pointer after approach and that
pointer-grab callbacks never synchronously reenter the Smithay pointer mutex.
Existing keyboard recovery must work with the shell absent. The installed-day
feel check and any deploy remain the owner's actions.

## Delivery order and remaining decisions

1. Repair notification publication and prove arrival on an owned nested display;
   extend PixelKit and bottom-right presentation on the same existing service path.
2. Make preview scheduling/painting strictly Home-visible-only; then add truthful
   tab metadata with a proven per-window binding for each supported application.
3. Implement the revised decoration sequence, which has the largest input/geometry
   regression surface. Preview capture and notification transport do not depend
   on that redesign; the title-tab badge can consume the same metadata later.

Prototype review points: title-always/controls-hover interpretation, opposite-end
versus diagonal opposite-corner layout, dwell/hysteresis feel, fullscreen title
policy, notification classification/long-job threshold, and application-specific
Wayland tab-count binding. Defaults above allow scoping to proceed without silently
claiming universal app integration or settling the ambiguous interaction by fiat.

## October 7 continuation review

The handoff's unit-green status did not establish end-to-end behavior. Review and
native nested execution found and repaired these additional gaps:

- **Previews:** actual clipped picture wells (including union occlusion), monotonic
  demand generations, immediate Off/lock retirement, exact capture-resource fencing,
  real live deadlines, and compositor window-ID lookup instead of Wayland object IDs.
  Shell restart recreates identifiers and demand authority; observer/screenshot
  connections cannot take it over. Unchanged content waits without another GPU
  render/readback. Demanded offscreen clients receive throttled frame callbacks
  without becoming foreground. The render scale now fits the whole client rather
  than cropping large windows. One context-tagged 512px render target is reused;
  adaptive SHM constraints reserve 4 MiB of pools, with bounded shell bitmap/ready
  storage under the shell's 16 MiB envelope; compositor scratch/pool storage is
  bounded separately below its own 16 MiB limit.
- **Tab counts:** removed public self-confirmation/PID claims. Sessiond consumes the
  authenticated native compositor inventory and publishes a fenced count feed.
  Count-only changes participate in broadcaster equality for both live and shelf
  projections. The available binding route requires the producer's actual bus PID
  to own exactly one native Wayland toplevel; ambiguity, disconnect, restart and
  window death invalidate it. X11 PID properties are not authority.
- **Notifications:** serialized commits and journal publication, atomic snapshot
  cursors, persisted expiry recovery, and an owner-bound shell journal consumer.
  Generation-aware replacement/overflow handling, scaled host/input geometry,
  bottom-right production placement and nonactivating hosts replace the old
  polling/fingerprint path. Queued arrivals have a separate 40-second freshness
  bound, not the 3/4/5-second visible dwell; the first batch admits three visible
  plus twenty waiting cards. Critical cards stack above fullscreen clients, while
  ordinary cards defer. Host size commits re-anchor growing/shrinking stacks.
  Bash/zsh prompt status preservation and real fish controlling-terminal execution
  were exercised.
- **Window tabs:** matching-release controls, one four-corner reveal/deadline
  authority, cancellation across privacy/visibility changes, bounded tiny/overlay
  layouts, opaque control glyphs with transparent gaps, and committed settings
  relays. Corner-only native motion produced no additional client configure events.
  XWayland windows now register on late surface association as well as map request,
  with one idempotent path for either ordering.

Actual evidence belongs in AKR; retained screenshots, private runtime logs and
scenario observations are under `.agent/scratch/homegarden-review-20261007/`.
Observed scenarios include native terminal/browser live previews, unchanged-frame
suppression, Off retirement, observer isolation, shell restart recovery, native
four-corner approach, Close press/outside-release cancellation, settings changes
  during a press, opposite-end layout and maximized controls. Additional owned
  runtime observations cover XWayland corner/resize/park/restore/close, the Settings
  Windows page, notification replacement/FIFO/owner restart/actions, critical
  fullscreen visibility, stack growth/shrink, and lock-time toast removal.

### Still required before acceptance

- Managed-unit positive tab-feed execution and real Kitty/browser adapters with
  exact per-window surface authority. Kitty's installed Wayland remote-control
  data has no native platform-window ID, and a browser extension's window ID does
  not identify its Wayland toplevel. No focus heuristic, fabricated daemon or
  self-asserted binding substitutes for that prerequisite; unknown counts remain
  absent. Isolated winit fixtures do not bypass production cgroup authentication.
- Full native/XWayland, fractional-scale, multi-output/hotplug, keyboard/recovery,
  overlap/tiny/group and lock/fullscreen acceptance matrix. A demonstrated subset
  is not a pass for the entire matrix.
- Owner review of title/fullscreen policy, opposite-end arrangement, dwell and
  hysteresis, notification classification/thresholds, and installed-day feel.
  Deployment and the installed-day check remain the owner's actions.

### HDMI crash investigation

The installed compositor's October 7 crash was examined offline, without restarting
the live session or provoking another hotplug. The retained core and previous-boot
journal show a 30-second event-loop stall followed by the watchdog's `SIGABRT`.
The main thread was in Mesa 26.0.8's `glFenceSync`, reached through Smithay texture
rendering during the vblank repaint. Earlier watchdog samples report
`D` / `folio_wait_bit_common`; kernel logs concurrently report slow NVIDIA hotplug
work. This establishes the failure path, not the underlying driver/IO cause.

The installed binary still ignores the secondary GPU; the working tree's multi-GPU
backend is newer and was not the crashing implementation. No speculative driver
workaround or watchdog suppression was applied. Exact Mesa debug symbols were not
available from the configured package indexes or Ubuntu's build-ID endpoint.
Physical HDMI reproduction with the reviewed build remains a separate owner/lab
gate; software-only nested rendering cannot establish hardware recovery.

## External protocol references

- [Kitty remote control: OS windows, tabs, panes and fine-grained permissions](https://sw.kovidgoyal.net/kitty/remote-control/).
- [Kitty remote-control wire protocol](https://sw.kovidgoyal.net/kitty/rc_protocol/#ls).
- [WebExtensions `tabs.query`, including per-browser-window selection](https://developer.mozilla.org/en-US/docs/Mozilla/Add-ons/WebExtensions/API/tabs/query).

These establish candidate metadata APIs, not a demonstrated binding to TemplarOS
Wayland toplevels. No browser extension or terminal adapter was run in this scope.
