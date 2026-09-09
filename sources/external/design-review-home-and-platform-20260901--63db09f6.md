# External design review — Home Field, file service, projects, app identity, resourced, hotfixability (2026-09-01)

I reviewed the current code, architecture notes, Home Field design, file-service design, project model, application identity path, resource governor, and hotfix/deployment planning in the agent pack.

The project does **not** need a different home-screen concept. The underlying Home Field model is already substantially better than the visible interface. What it needs now is subtraction, consolidation, and a clearer rule for how an object grows from a glanceable representation into a working surface.

## The central design rule

I would make this the governing principle for the shell:

> **One object, one canonical route, many sizes of representation.**

A project should not be simultaneously represented by a Projects drawer, a project tile, its application tiles, and its session tiles unless those representations convey genuinely different information. A folder should not turn into a different UI when enlarged. A small Files surface should become a larger Files surface and finally a full browser, preserving location, selection, and navigation state.

That distinction is already implicit in `docs/homefield.md`, particularly the idea that **the launcher is inventory while Home is working memory**. I would push that much harder.

The other useful distinction is:

> **Do not minimize empty space. Minimize useless space.**

A small amount of empty Home Field is useful because it gives you somewhere to drop, throw, rearrange, or create objects. A permanent 34-pixel title band on a 104-pixel tile is not useful empty space. Eight permanent destination tiles that mostly lead elsewhere are not useful space. Two search boxes in the same navigation hierarchy are not useful space.

That is where most of the current density can be recovered.

---

# 1. The Home screen should stop being a collection of destinations

The biggest current problem is in `shell/lege-shell/src/field_model.rs`.

The default Home gets eight drawer objects:

Downloads, Recent, Projects, Drives, Operations, Trash, Documents, and Media.

At the same time, the field separately materializes actual projects, running/parked sessions, system widgets, trays, and so forth.

This recreates a conventional launcher/file-manager taxonomy on top of the more interesting spatial model.

I would reduce the default Home to essentially three kinds of things:

**Work** — actual projects, active/parked sessions, and user-created trays.

**Files** — one adaptive information surface encompassing file navigation, pinned locations, drives, recent items, operations, trash, documents, media, downloads, etc.

**System** — a very small number of adaptive status/control surfaces for notifications and machine state.

The application catalogue remains available from the omnibox/launcher, but it isn't normally occupying Home at all.

This one change would make Home substantially calmer while making it *more* capable.

### There are several concrete redundancies now

| Current presentation                         | Problem                                                        | Better model                                                                |
| -------------------------------------------- | -------------------------------------------------------------- | --------------------------------------------------------------------------- |
| `Projects` drawer + real Project tiles       | Same conceptual object twice                                   | Real projects are Home objects; project inventory lives in search/inventory |
| App badge + session tile                     | A running app can appear both as app and actual window/session | Session represents running work; app tile only when explicitly pinned       |
| Launcher widget + global app search          | Both lead to application inventory                             | Omnibox is the canonical app entry point                                    |
| 8 file-related drawer tiles                  | Destinations consume more area than information                | One adaptive Files surface                                                  |
| Expanded drawer + inner legacy drawer rail   | Navigation is literally nested twice                           | Expanded object renders only its content                                    |
| Home search + inner drawer search            | Two search models in one view                                  | One omnibox whose scope follows context                                     |
| LIVE/PARKED counts + visible session objects | Summarizes something already visually present                  | Show only when sessions aren't otherwise visible/relevant                   |

The field is already capable of representing the actual objects. It should stop surrounding them with proxies for those objects.

---

# 2. Files should become the showcase for “small widget → big widget”

This is probably the most important UI to build correctly because it solves your requirement that essentially the whole filesystem remain quickly navigable without covering Home with folders.

I would make **Files one persistent semantic object with several levels of detail**.

### Small Files

It might occupy something like 2×2.

It should show genuinely useful state rather than buttons:

* free/used storage,
* perhaps the latest meaningful file,
* active transfer/operation if there is one,
* two or three pinned locations or recently used locations.

It should not say giant things like `FILES`, `FOLDER`, or `DRIVE` when shape, iconography and content already communicate that.

### Wide Files

Give it enough area and it becomes a lightweight browser:

Pinned/important locations on one side and recent/changed files on the other.

Downloads only need prominence when something downloaded. Trash only needs prominence when relevant. A long-running copy gets a progress row automatically.

The point is that information claims space because it currently matters rather than because the taxonomy says the category must permanently exist.

### Large Files

Now it becomes an actual navigable file surface:

breadcrumb, children, previews, sorting, selection, storage information, perhaps a secondary information pane if enough width exists.

The user can navigate several directories deep **without launching another application**.

### Full/zoomed Files

The exact same object grows into the full file-management experience.

This is where the complete browser, search results, detailed operations, properties, previews, etc. belong.

It should still feel as though you enlarged the thing you were using rather than opened a separate program that happens to show the same directory.

Your `Files.Browse1`, `Files.Search1`, volume service, recent history, operation journal and trash architecture already support most of this. The missing work is primarily shell composition.

---

# 3. There is a specific implementation problem to remove

`field_screen.rs` currently expands a drawer by embedding `HomeSurfaceScreen`.

But `HomeSurfaceScreen` is itself the old complete Home/navigation surface. It draws its own drawer rail and its own search field.

So the hierarchy effectively becomes:

**Home top bar → expanded Home object → another Home navigation rail → another search bar → actual contents**

That is exactly the sort of presentation redundancy you want to eliminate.

I would retire `HomeSurfaceScreen` as a complete embedded screen.

Extract the useful file-browser/content pieces underneath it and make those reusable rendering components. `FileBrowserScreen` is much closer to the thing the field actually needs.

Then an expanded Files tile becomes:

**one breadcrumb + one content surface**

not another desktop within the desktop.

The old `Drawer` enum can survive internally for a while as named routes/queries:

`Downloads`, `Media`, `Trash`, etc.

But these should become **locations or filters inside Files**, not top-level Home object types.

Eventually, even the UI ought not care much whether “Downloads” came from a special enum, a standard XDG directory, or a user-pinned location. They're all locations.

---

# 4. Make the omnibox the universal route into everything

The top Home search currently behaves mostly like an application launcher. In some expanded drawer cases there is separate file searching.

This is much too weak for the architecture underneath it.

The single search/command field should federate, locally:

**Applications** from the already-cached application inventory.

**Projects** from the project service/cache.

**Files and directories** asynchronously from `Files.Search1`.

**Commands/actions** from declared shell/application actions.

It does not require building another giant search daemon. The shell can merge results from existing authorities.

Typing:

`court`

might give:

> Court Simulation — Project
> Save Your Skin — application/window
> court_layout.md — file
> court-reference — directory

Results should visibly indicate type, location/project membership, and relevant state, but shouldn't overwhelm the user with metadata.

When the omnibox is invoked from inside a large Files surface, it can initially be scoped to that directory, with an obvious escape to Everything. Inside a project, its first results can be project members.

There should still be **one search control**.

This also means the Launcher system tile can disappear from the normal default field.

---

# 5. The current representation ladder should become the design language

`crates/lege-field/src/tile.rs` already has the important primitive:

`Micro → Small → Wide → Large`

with different tile kinds allowed different spans.

That is more interesting than Windows 8 tiles.

I would make this the defining visual/interaction concept of LegeOS:

> **Objects reveal capability as physical area becomes available.**

Not merely “the same widget but bigger.”

For example:

**Project**
Small: name, activity/state.
Wide: active sessions, key files or tasks.
Large: miniature project workspace.
Zoomed: entire project field.

**Session**
Small: icon/title/status.
Wide: live preview.
Large: larger preview plus relevant session actions.
Zoomed/open: actual window.

**Notifications**
Small: attention indicator/count.
Wide: two or three most important items.
Large: notification history/actions.

**System**
Small: only exceptional or high-value state.
Wide: battery/audio/network/storage.
Large: controls and diagnostics.

The important missing piece is **state continuity**. If I navigate Files to:

`Projects/Lege/src/shell`

and then enlarge it, I should still be at that exact location with the same selection. Shrinking it should preserve it again.

The current expanded-drawer architecture effectively changes UI objects at this transition. I would instead give every expandable surface a persistent state model, something conceptually like:

`FilesSurfaceState { location, back_stack, selection, sort, view_mode, query }`

and then let Small/Wide/Large/Full renderers consume the same state.

That would make expansion feel physical.

---

# 6. Representation should be shape-aware, not merely area-aware

One change I'd make to the field primitive itself:

`Representation::for_span` is currently heavily based on **cell count**.

But a 4×1 object and a 2×2 object both occupy four cells and have completely different information-display capabilities.

A horizontal list works beautifully in one and badly in the other. A preview may want a roughly rectangular minimum.

So descriptors should eventually declare requirements such as:

* minimum columns,
* minimum rows,
* desirable aspect ratio,
* minimum effective content pixels.

The representation selector can still remain simple and deterministic, but should consider `(cols, rows)`, not just `cols × rows`.

This becomes particularly valuable when the field is trying to consume otherwise-unused space.

---

# 7. Recover space from chrome before packing tiles more tightly

`paint_tile` currently reserves roughly a 34-pixel grab/title band.

On a 104×104 single-cell object, that's an enormous portion of the entire display.

I would use different chrome at different representation levels.

A Micro/Small object gets perhaps a narrow edge affordance and a title integrated into its content. Hover/focus can reveal its full manipulation affordances. Touch could use a short hold or dedicated manipulation mode.

Wide/Large objects can afford a conventional 28–32px heading region.

Likewise, don't give every tiny system widget a permanent control strip. Small mode is predominantly **information** with perhaps one obvious primary action. Wide mode gets a few inline controls. Large gets the complete controls.

This is where the widget contract's “information first, actions declared separately” rule is valuable.

---

# 8. Don't mathematically fill the Home screen

I would resist a Windows-8-like requirement that every pixel belong to a rectangle.

The current planning is right to value stable placement and insertion space.

Instead:

**At first layout**, adaptive surfaces can expand to consume a reasonable amount of slack.

**After the user arranges things**, their geometry becomes stable. A refresh never causes a dashboard-style reflow.

Empty space becomes an implicit “throw target”: an obvious place where something new can land.

The home should therefore feel dense because there is little redundant chrome and every visible object says something, not because there are no gaps.

That distinction will keep it from becoming another tile dashboard.

---

# 9. A distinct LegeOS design language

I would borrow only three things from Windows 8:

stable rectangular regions, information at a glance, and direct manipulation.

I would not borrow the sea of equally assertive colored tiles or the application-centric hierarchy.

LegeOS can instead use **calm rectangular instruments**:

Neutral substrate, restrained boundaries, low corner radius, consistent narrow gutters, typography doing much of the hierarchy, and accent/color primarily communicating *state* rather than application branding.

A project shouldn't be purple because “projects are purple.” It might acquire an accent when active, waiting, failed, syncing, or selected.

Likewise, the interesting visual texture should be **inside** objects. Your planning's rule that treemap/cushion layouts belong inside storage/file representations is exactly right.

Then the Home surface stays visually calm while its objects can be internally rich.

Most importantly, expansion should be spatial. A tile should visibly grow from where it was, and collapse back into precisely that location. That continuity can become far more identifiable as “LegeOS” than copying a recognizable tile skin.

---

# 10. Projects need to become semantic objects, not just field containers

There is an architectural separation worth preserving:

**`HomeField1` owns presentation and spatial state.**

**Projects should own semantic project membership.**

The current core `Project` structure already knows more than the public project API exposes: root, directories, apps, environment, terminal directories, mounts, `priority_boost`, etc.

But the D-Bus surface does not expose much of that richness.

I would expand `Projects1` compatibly or introduce `Projects2` so projects can authoritatively own:

directories/references, applications, environment, mounts, terminal roots, selected/current project state, recent activity, and eventually user importance.

Then Home stores:

> this project is here, this size, displaying these aspects.

It should **not** become the only record that these files/apps belong to that project.

This also lets projects exist cleanly when not visible on Home. Archived or inactive projects remain searchable through the omnibox without consuming a permanent `Projects` drawer.

`priority_boost` also appears to be currently inert. Either wire it into actual resource policy as a bounded hint or remove it until it has semantics. An inert “priority” setting is worse than no setting because it implies guarantees the system doesn't deliver.

---

# 11. App origin unification needs two identities, not one

The present AppId design establishes an excellent invariant for runtime ownership, but packaging/origin is being compressed too aggressively.

The correct abstraction is:

**Logical application identity** — “Firefox”.

**Installed variant identity** — “the Firefox Flatpak from Flathub”, “the distro Firefox package”, “a development build”, etc.

I would introduce something along the lines of:

`AppId` / `LogicalAppId`

and

`AppVariantId`

An installed variant should carry provenance such as package/origin kind, package/ref/version, desktop entry, launch mechanism, executable, sandbox/trust boundary, declared privileges, provider claims and update owner.

Then the ordinary UI can show **Firefox once**.

If two variants are installed, its details/context menu can say:

> Default: distro package
> Also installed: Flatpak

and let the user choose.

Running instances should record both logical app and launch variant.

This matters enormously for security. You should **not** unify permissions merely because two executables represent the same human concept. A trusted LegeOS package and an arbitrary user-installed `.desktop` file must not automatically inherit each other's provider rights or permissions.

So:

> unify presentation; preserve provenance.

Associations can normally point to the logical app, plus an optional preferred variant. The user says “PDFs open in Foo”; the platform can remember which Foo installation is preferred without exposing package plumbing constantly.

This is cleaner than letting `desktop-import.*` effectively become the permanent semantic identity of arbitrary installed software.

---

# 12. The memory governor is already one of the better-designed modules

I would **not** replace the current resource architecture.

Keeping `lege-resourced` tiny, separate and capable of operating without SQLite, filesystem writes or D-Bus dependencies in its emergency loop is the right design.

The current progression:

Normal → Elevated → Severe → Critical

and

Observe → Trim → Throttle → Checkpoint → Terminate

is also much more defensible than a simple largest-process killer.

I would improve it in a few specific ways.

First, don't make one score choose one victim at every stage. Memory reclamation works better as a **reclaim plan**. At Elevated/early Severe, several background/disposable applications can be asked to trim before the system starts squeezing one process repeatedly.

Second, improve the notion of “safe to sacrifice.” Recency, last interaction, project importance, checkpoint capability/freshness, and a user pin/protection state are highly relevant. `priority_boost` could eventually feed this as a modest modifier.

Third, treat `has_unsaved_work = false` conservatively. If that answer came from an application a long time ago, it isn't a guarantee anymore. Unsaved-state/checkpoint information needs freshness.

Fourth, reconsider Severe treatment of the foreground application. The scoring penalty helps, but Severe can still end up applying `memory.high`/reclaim to it if its score wins. I would make foreground throttling much harder to reach except when it is demonstrably the runaway source or pressure is genuinely Critical.

Fifth, the fixed global 3 GiB reserve/1 GiB critical thresholds need trace-based scaling across machine sizes. A 4/8 GiB machine and a 64 GiB workstation shouldn't use identical absolute policy. PSI already helps, but total-memory-aware bounds would make the policy less hardware-specific.

Sixth, explicitly decide what happens with **systemd-oomd**. I did not see coordination with it in this tree. You do not want two independent PSI-driven mechanisms racing, with systemd-oomd killing something before the carefully explainable LegeOS path completes. Either divide ownership of slices deliberately or configure one as a defined last-resort backstop.

And consider modest `memory.low` protection for the state/file infrastructure needed to recover from pressure, particularly `lege-sessiond` and `lege-filed`, assuming their slice arrangement makes that useful. They're not candidates in the `app.slice` victim scan, but kernel reclaim is a separate issue.

The current observe-only posture before trace validation is correct. I would keep termination disabled until replay tests demonstrate predictable behavior.

---

# 13. Resource management should surface itself only when something matters

Don't spend permanent Home space on a RAM monitor.

That would violate the information-density principle because 99% of the time it says “everything is fine.”

Instead, resource pressure should **promote information when exceptional**:

A subtle system indication at Elevated pressure.

A visible status on an app/session if it is being throttled.

A checkpoint indicator if LegeOS preserved state.

A concise explanation after intervention:

> Memory pressure became critical. Foo was checkpointed and suspended, recovering 3.1 GB.

Clicking that can open the detailed resource view.

This is another application of small → big: normal state consumes almost nothing; exceptional state gets attention; investigation expands into detailed telemetry.

---

# 14. Hotfixability should be a first-class platform feature

The planning in `hotfixability-current-state.md` is directionally right.

The key is not “can I overwrite files under `/usr` while the OS is running?”

You obviously can.

The harder question is:

> **Can the installed system identify exactly which version is running, safely activate a new one, determine whether it converged, and get back to the previous working system if it didn't?**

For your intended development model, I would create **two update lanes**.

### Live userspace activation

This is the loop you use constantly while developing the installed distro.

Build a complete coherent package set first. Give the entire build a unique `BuildSetId` containing a monotonic package revision plus Git revision/dirty state.

Put successful sets into a local signed APT repository rather than doing ad-hoc `dpkg -i` followed by dependency repair.

Then an activation tool can know:

> installed set: A
> running shell: A
> running sessiond: A
> candidate set: B

Each long-lived LegeOS component should expose its running build identifier.

A deployment can then deliberately activate components according to boundary:

| Component/change               | Normal activation                                    |
| ------------------------------ | ---------------------------------------------------- |
| `lege-shell`                   | Restart shell, health-check                          |
| Ordinary applications          | Next launch/restart                                  |
| `lege-resourced`               | Controlled service restart                           |
| `sessiond`, `filed`, portal    | Restart only after reconnect/version-skew guarantees |
| compositor / Wayland protocol  | New session/logout-login                             |
| kernel, NVIDIA, firmware, boot | Boot deployment/reboot                               |

This is much safer than pretending everything can hot-reload equally.

The shell is particularly well positioned for this because `HomeField1` now persists field state in `sessiond`. A shell crash/restart should be treated almost like reloading a browser tab: inconvenient at worst, not destructive.

### Boot deployment

The `lege-hostd` deployment lifecycle and Btrfs planning are the correct second lane for risky changes.

The fresh installation should create a **known-good factory/base deployment** that subsequent development never destroys.

Home and mutable state live separately. New root deployments are staged and verified, then booted as trials. Promotion happens after health checks. Failure returns to the previous deployment.

That gets you to the thing you're actually asking for:

> install LegeOS once, then use LegeOS itself as the development environment for improving LegeOS.

Not because `/usr` is mutable forever, but because the machine has a reliable path for applying both lightweight and heavyweight changes.

---

# 15. Make live rollback include state compatibility

There is one trap here: rolling binaries back while leaving their databases migrated forward.

A perfect package rollback is useless if yesterday's `sessiond` cannot read today's database.

For stateful services, development activation needs one of these policies:

Backward-readable schema changes for at least one supported rollback generation; explicit schema-version compatibility; or snapshots of daemon state before a breaking upgrade.

I strongly favor keeping migrations additive/backward-readable during active development wherever possible.

Likewise, D-Bus evolution should be additive. Breaking interfaces get a new interface version and an overlap period. Shell A and sessiond B should either interoperate or fail a compatibility check **before** activation.

`FieldDocument` also deserves an explicit migration registry eventually rather than relying indefinitely on strict current-format loading. A shell upgrade should never turn someone's carefully arranged Home into unusable serialized state.

---

# 16. The existing reinstall script is not yet this update system

`scripts/reinstall-legeos-desktop.sh` is useful development scaffolding, but it currently:

builds a package batch, installs it with `dpkg`, lets apt repair dependencies, reloads systemd, and runs the doctor.

That isn't atomic and it doesn't tell you which installed binaries are actually executing afterward.

I would evolve that workflow into a real `lege dev deploy`/`lege update` mechanism instead of accumulating shell-script special cases.

A successful activation should end with something equivalent to:

> Candidate build B installed
> shell: B ✓
> sessiond: B ✓
> filed: B ✓
> resourced: B ✓
> compositor: A — activation requires new session
> kernel: A — unchanged
> persistent state: schema compatible ✓
> previous build A retained for rollback

That kind of visibility will be much more valuable during self-hosted development than sophisticated hot-module replacement.

---

# What I would change first

The sequencing matters because there is a temptation here to add more widget types before the core composition is correct.

1. **Refactor the Home presentation before adding anything else.** Remove `Projects` as a default drawer, stop auto-creating redundant App tiles for represented sessions, collapse the eight default file drawers into a single Files object, and remove the nested `HomeSurfaceScreen` from expanded-field rendering.

2. **Build the adaptive Files surface and federated omnibox.** This proves the small → wide → large → full model on the hardest general-purpose information source and gives every file/project/app a fast global route.

3. **Make representation stateful and shape-aware.** Expansion must preserve selection/location, and representation selection should take `(cols, rows)` into account. Then reduce Small/Micro tile chrome aggressively.

4. **Finish project semantics.** Expand the project service so semantic membership is authoritative outside HomeField, then let real project tiles become adaptive miniature workspaces. Retire the Projects drawer completely.

5. **Introduce logical app + variant/origin identity.** Do this before permissions/provider/update semantics become deeply attached to today's one-dimensional AppId model.

6. **Turn package replacement into an actual activation system.** Build-set IDs, local package repository, installed-versus-running convergence, service-specific restart rules, state/API compatibility tests, and Btrfs boot deployments for everything that cannot safely activate live. In parallel, keep `resourced` observe-only, add trace/replay testing, define systemd-oomd ownership, and gradually enable active memory policy.

The most significant point is that **the project is much closer architecturally than the present Home UI makes it appear**. `lege-field`, `HomeField1`, the file service and the widget contract already contain most of the primitives I would have proposed from scratch. I would therefore avoid a frontend rewrite. The next iteration should mostly delete redundant concepts, extract the legacy file-browser content from its old navigation shell, and make the existing field's representation ladder the organizing principle of the entire interface.
