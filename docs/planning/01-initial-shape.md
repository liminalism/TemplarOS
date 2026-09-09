# My assessment

There is substantial room to build a better Linux desktop operating system around the problems you identified. The useful project, however, is **not a new kernel, libc, init system, package format, or application ecosystem**. Those would consume most of the work while contributing little to your actual goals.

The stronger design is:

> **A tightly integrated desktop OS built on an Ubuntu LTS core, with its own compositor policy, shell, file layer, application identity system, resource governor, driver/update transaction system, and installation image.**

That would legitimately become a new distribution once it had its own repository, installation image, updater, defaults, release policy, and hardware-validation process. It can begin as a replacement desktop session installed on Lubuntu.

The governing principle should be:

> **Start from scratch in policy and user interaction, not in plumbing that already works.**

---

# First: Linux 7.2 and your Lubuntu installation

Linux 7.2 is the upstream **kernel**, not a new version of the whole Linux operating system. Kernel.org lists Linux 7.2 as released on **August 16, 2026**, currently in its mainline slot, while 7.1.8 is listed as the stable series. Lubuntu does not automatically adopt every kernel.org release. Canonical selects, patches, packages, tests, and distributes particular kernels through Ubuntu releases and the Hardware Enablement stack. Ubuntu 26.04 LTS currently uses kernel 7.0. ([Kernel.org][1])

Your migration document identifies the installation as **Lubuntu/COSMIC on an Ubuntu 25.10 base**.  If that is still the installed version, it is already out of support: Ubuntu 25.10 reached end of life on **July 9, 2026**, and no longer receives normal package or security updates. The supported upgrade path is Ubuntu/Lubuntu 26.04 LTS, which Lubuntu supports until April 2029. ([Lubuntu][2])

You can verify the current state with:

```bash
grep '^PRETTY_NAME=' /etc/os-release
uname -r
hwe-support-status --verbose
```

I would upgrade the base to 26.04 before doing more distribution work. I would **not** manually place upstream Linux 7.2 on your primary installation merely because it is newer. That is particularly unwise on a hybrid Intel/NVIDIA laptop after already encountering conflicting-driver problems. It would also undermine the exact driver validation and rollback model your new OS should enforce.

A newer kernel can improve particular drivers, schedulers, filesystems, or hardware support, but it does not inherently make the desktop faster. Your experience with CachyOS is unsurprising: scheduler patches cannot fix fragmented application identity, poor memory-pressure policy, compositor geometry bugs, repeated file-handler prompts, or weak driver transactions.

---

# You are already partway toward a distribution

Your migration guide is not merely a collection of themes and aliases. It contains modifications spanning:

* A replacement NetworkManager tray and automatic timezone service.
* XDG autostart conflict resolution.
* COSMIC application-library behavior.
* COSMIC Files state restoration.
* The COSMIC file-chooser portal.
* Dock/window identity and grouping.
* LXQt packaging workarounds.
* Dolphin and generic file-manager integration.

Your COSMIC changes already occupy four separately maintained submodules, and your notes document the rebase and detached-submodule problems involved in carrying them forward.  Your file manager and portal changes also show that “remember the last folder” cannot be solved cleanly in one GUI alone; the file manager and file-chooser service must share a consistent system policy. 

That is the real evidence in favor of making a coherent OS layer. You have reached the point where local scripts and unrelated forks are becoming less maintainable than a set of properly packaged system components with stable interfaces.

The distinction is:

| Project level               | What it owns                                                               | Assessment                  |
| --------------------------- | -------------------------------------------------------------------------- | --------------------------- |
| Personal customization      | Dotfiles, scripts, patched applications                                    | Already exceeded            |
| Desktop session             | Shell, compositor configuration, file manager, settings                    | Immediately feasible        |
| Ubuntu-derived distribution | ISO, package repository, updater, hardware policy, defaults                | Appropriate eventual target |
| Foundational distribution   | Kernel maintenance, libc, package archive, toolchain, general driver stack | Poor use of effort          |

The file manager by itself does not justify a distribution. The combination of a new compositor policy, resource governor, application contract, association broker, NVIDIA transaction model, and updater does.

---

# The central innovation: one application contract

The most important architectural problem in the Linux desktop is that the same application can have several unrelated identities:

* A package name.
* A `.desktop` file ID.
* A Wayland `app_id`.
* An X11 `WM_CLASS` or `StartupWMClass`.
* A Flatpak ID.
* An executable path.
* A D-Bus name.
* A Steam application ID.
* A Wine executable and prefix.
* One or more process IDs.

Your dock patch already had to match pinned IDs and `StartupWMClass` so that Flatpak, Steam, and Wine windows would not appear as duplicate applications.  That same fragmentation is implicated in file associations, launch behavior, resource accounting, permissions, notifications, crash recovery, and autostart.

The new OS should introduce an **application registry and launch broker**, tentatively `appd`.

For every application, `appd` would maintain a canonical application ID and its known aliases. For every launch, it would produce a cryptographically unguessable or otherwise verified **application-instance token**. It would then:

1. Create a dedicated cgroup for the application instance.
2. Start the process in that cgroup.
3. pass an activation token to the compositor.
4. Associate every resulting window with that instance.
5. Associate file-handler capabilities and permissions with the canonical application.
6. Attribute notifications, crashes, memory use, disk I/O and background activity to it.
7. Store enough session information to reopen it later.

The compositor should not blindly trust a Wayland client’s self-declared application ID. It should reconcile that declaration with the launch token, executable, package ownership and cgroup.

This one service would give the system a stable answer to:

* Which application owns this window?
* Which process group should be throttled or terminated?
* Which icon represents it?
* Which files may it handle?
* Which GPU did it request?
* May it run in the background?
* Is another instance already running?
* What state should be restored after a crash?
* Why did the system select it to open this file?

It also offers a clean solution to your duplicate network applets. Your existing installation required an XDG override to stop both `nm-applet` and your replacement `nm-tray2` from starting.  Under the proposed system, both would declare that they provide the exclusive `network-status-ui` capability. The registry would allow exactly one selected provider to autostart.

That is much stronger than fixing each duplicate icon, applet, file association and process-accounting bug separately.

---

# Proposed system architecture

```text
┌──────────────────────────────────────────────────────────┐
│ Home Surface │ Files UI │ Settings │ Recovery Overlay    │
└───────────────────────┬──────────────────────────────────┘
                        │
                Shell and session policy
                        │
┌───────────────────────┴──────────────────────────────────┐
│ appd: identity, launching, instances, permissions        │
│ association service: file and URI handlers               │
│ filed: mounts, search, file operations, history          │
│ resourced: cgroups, PSI, priorities, pressure response   │
│ updated: staged system and driver updates                │
└───────────────────────┬──────────────────────────────────┘
                        │
        Stable Wayland display/compositor core
                        │
           Mesa / NVIDIA package integration
                        │
              DRM/KMS and Linux kernel
                        │
          Ubuntu 26.04 LTS system foundation
```

I would initially retain systemd, cgroup v2, APT/Debian packages, standard Linux filesystems, PipeWire, NetworkManager, BlueZ, Mesa and the upstream NVIDIA userspace stack. None of your major complaints requires replacing those.

---

# 1. Eliminate the desktop surface, not the desktop system

Your proposal makes sense once “desktop” is separated into two meanings:

* The **desktop session** is the compositor, shell, input management, panels, notifications and window policy. You still need that.
* The **desktop surface** is the wallpaper-plus-icons folder exposed behind windows. That can disappear completely.

I would replace the desktop surface with a shell-owned **Home Surface** or **Navigator**. It would appear whenever no normal window occupies an area, or when explicitly invoked. It would not be another application window that can get lost behind other windows.

The default surface could contain:

| Drawer            | Contents                                                          |
| ----------------- | ----------------------------------------------------------------- |
| Recent            | Recently used files, applications and projects                    |
| Projects          | Explicit working sets with files, terminals and application state |
| Downloads         | Current transfers and recent downloads                            |
| Documents / Media | Standard paths and saved searches                                 |
| Drives            | Mounted volumes, free space, health and eject controls            |
| Network           | Network shares and remote machines                                |
| Operations        | File copies, conversions and background jobs                      |
| Trash             | Contents, size and restore actions                                |

Every drawer would be searchable. A drawer could represent a real directory, a mount, a saved query, a tag, or a project. The underlying files should remain normal POSIX files at visible paths. Do not invent a proprietary object filesystem that breaks terminal tools and third-party applications.

`~/Desktop` should remain as a compatibility directory because applications may refer to it, but the shell would not render its contents automatically. A wallpaper could remain an optional visual setting, disabled by default rather than treated as a functional layer.

A useful related change would be to make **minimization explicit**. A minimized window should go to a visible Window Shelf or Recent drawer. It should never silently vanish into an icon, another workspace, or an output that is no longer attached.

---

# 2. File manager and file service

Your fork of COSMIC Files is the sensible starting point. Rewriting all file-manager functionality would add months of edge cases without helping the core design. The important change is to stop treating the file manager as a monolithic GUI.

Create a separate `filed` service responsible for:

* Mount discovery and volume metadata.
* File indexing and search.
* Copy, move, rename, delete and trash operations.
* Progress reporting and cancellation.
* Durable operation journals.
* Conflict handling.
* Recent-file history.
* Last-used directories.
* File previews and metadata.
* Association lookup through `appd`.

COSMIC Files, the Home Surface and the file-selection portal would become clients of this service.

## Storage information should be unavoidable

For the currently displayed directory, the UI should always show:

* Containing volume.
* Free, used and total space.
* Space reserved for the system.
* Read-only or degraded state.
* Filesystem type.
* Current pending operation size.

The sidebar should show remaining space beside every drive. The Home Surface drive cards should do the same. A user should not have to open a separate disk utility to discover that a drive is nearly full.

The OS should also reserve enough space for logging in, updating and recovering. When a volume approaches that reserve, applications should be prevented from consuming the last usable blocks without an explicit override. A full root filesystem can be almost as destructive to responsiveness as exhausted RAM.

## File operations should be transactions

A long copy or cross-volume move should survive a file-manager crash. A robust move would be:

1. Copy to a temporary destination.
2. Flush required metadata and data.
3. Verify expected size and, optionally, a checksum.
4. Atomically expose the destination where possible.
5. Delete the source only after destination completion.
6. Record enough information to explain or reverse the operation.

Undo should work for rename, move and trash operations. Operations that cannot be safely reversed should say so before they run.

Your existing “Copy Path” integration exists because neither Dolphin’s built-in behavior nor downloaded service menus worked reliably.  Under this architecture, “copy canonical path,” “copy URI,” “copy shell-escaped path,” and “reveal in terminal” would be first-class file-service actions rather than per-file-manager scripts.

## File picker and file manager must share state

Your separate COSMIC Files and portal patches currently remember their own last directories.  In the new design, applications would ask the portal for a file, while `filed` would supply:

* The last directory used by that application.
* The last directory used for that operation type.
* The current project directory.
* Any permission restrictions.
* A general fallback directory.

This would eliminate much of the repetitive navigation that currently requires local patches.

---

# 3. Compositing, monitors, overlays and windows that disappear

This is one of the components that can justify a new session.

Under Wayland, the compositor is the display server. It owns the scene graph, receives input from the kernel, determines which surface receives that input and composes the final screen. Consequently, the behavior you want—window placement, monitor transitions, overlays, focus and recovery—is correctly implemented in the compositor and shell rather than in individual applications. ([Wayland][3])

I would not begin from raw Wayland, DRM and input APIs. Smithay provides Rust building blocks for common compositor functionality, although it deliberately is not itself a complete compositor. ([GitHub][4]) The practical paths are:

1. Fork `cosmic-comp` and systematically remove or replace its workspace and placement policy.
2. Build a smaller Smithay compositor while using `cosmic-comp` as a compatibility and implementation reference.

The first path is more likely to produce a usable system. The second may eventually provide a cleaner core, but it has a much larger initial correctness surface.

## One workspace should be a real invariant

Do not merely hide workspace controls while retaining several invisible workspaces underneath. Configure the compositor to expose exactly one workspace, including to XWayland compatibility interfaces.

Multiple monitors would form one continuous scene. A window belongs to that scene, not to a particular hidden desktop. Changing monitors should only alter which outputs display portions of the scene.

## Geometry invariants

After every drag, monitor reconfiguration, scale change, resume and application geometry request, enforce rules such as:

* Every normal window intersects at least one active output.
* At least one usable recovery region—title area, border tab or equivalent—remains visible.
* Removing an output migrates its windows deterministically to the nearest remaining output.
* A drag anchor stays under the pointer when crossing outputs with different scale factors.
* Restored geometry is validated against the current output topology rather than trusted blindly.
* Full-screen and modal windows always have a compositor-level escape route.
* A global “show every window” recovery view works without relying on the ordinary shell.

These should be tested as properties rather than as a list of known bugs. Feed randomly generated output arrangements, scale factors and window rectangles through the geometry engine and assert that windows remain recoverable.

## Separate the stable display core from the shell

A Wayland compositor crash disconnects clients, so the compositor core must be exceptionally conservative. The launcher, Home Surface, panels, drawers and notifications should run in a separate shell process using a private authenticated protocol. A shell crash could then be restarted without terminating all applications.

The core should retain only:

* Wayland and XWayland client connections.
* Input routing.
* Output management.
* Scene graph and rendering.
* Window identity and geometry.
* Secure lock and emergency recovery surfaces.
* A minimal fallback window switcher.

Everything decorative should be restartable.

## Typed overlay layers

Instead of allowing arbitrary “always on top” behavior, use defined layers:

1. Secure lock and authentication.
2. Emergency system recovery.
3. System-modal requests.
4. Launcher, switcher and transient shell overlays.
5. Notifications.
6. Application always-on-top surfaces.
7. Normal windows.
8. Home Surface.

This makes z-order explainable and prevents accidental overlay conflicts.

---

# 4. Native NVIDIA handling

Your guide references an Intel Iris Xe plus NVIDIA RTX 4060 setup.  NVIDIA recommends its open kernel-module flavor on Turing and newer GPUs, and since the 560 series that flavor has been the suggested default. NVIDIA also states that the open and proprietary kernel-module flavors are mutually exclusive and should not coexist on the filesystem. ([NVIDIA Docs][5])

The distribution should therefore have **one driver authority**:

* The package repository owns all NVIDIA files.
* The installer selects one supported flavor based on detected hardware.
* NVIDIA’s standalone installer is not part of the supported configuration.
* Random third-party driver repositories are rejected or clearly move the installation into an unsupported state.
* Kernel, module and userspace versions are version-locked as a tested set.
* Hybrid, integrated-only and discrete-only modes are represented by one declarative setting rather than hand-edited configuration files.

## Treat a kernel/driver update as one transaction

A safe update sequence would be:

1. Download the candidate kernel, NVIDIA modules, userspace components and firmware.
2. Install them into a non-default boot deployment.
3. Verify module signatures, version matching and initramfs contents.
4. Preserve the complete previous deployment.
5. Boot the candidate with a watchdog.
6. Verify that the module loaded, DRM modesetting initialized, the compositor started, and Vulkan/OpenGL probes succeeded.
7. Promote the candidate only after successful validation.
8. Roll back automatically if the graphical session does not reach a healthy state.

This is much more valuable than immediately providing Linux 7.2. A distro that runs kernel 7.0 reliably is better than one that advertises 7.2 but occasionally boots to a black screen.

For your laptop, the hardware test matrix should specifically include:

* Intel-driven desktop with NVIDIA render offload.
* NVIDIA-driven desktop.
* External monitor attached to each available output path.
* Suspend and resume.
* Kernel update and rollback.
* Driver update and rollback.
* Wayland and XWayland applications.
* CUDA and Vulkan applications where installed.

---

# 5. Resource management and prevention of whole-system hangs

This is the strongest potential differentiator.

The problem is usually not merely that an application eventually exhausts memory. The desktop can become unusable earlier while the kernel is repeatedly reclaiming pages, swapping, faulting them back in and waiting for storage. By the time an OOM decision occurs, the user may already have experienced a long apparent freeze.

Linux already exposes the required mechanisms through cgroup v2:

* `memory.low` and `memory.min` protect important workloads from reclaim.
* `memory.high` throttles and applies reclaim pressure to an offending cgroup without immediately invoking OOM.
* `memory.max` provides a hard final limit.
* `memory.oom.group` allows an application workload to be terminated as one unit rather than leaving random helper processes behind.
* Per-cgroup pressure information and system-wide PSI allow a userspace monitor to respond to actual memory, CPU and I/O stalls. ([Kernel Documentation][6])

The missing part is a desktop-aware policy engine.

## `resourced`

Every application launched through `appd` should have a dedicated cgroup. `resourced` would continuously classify it as:

* Foreground interactive.
* Visible but not focused.
* Hidden/background.
* Batch or compute-heavy.
* System-critical.
* User-protected.
* Disposable/restartable.

The classification changes weights and protections; it does not require moving the application between invisible desktops.

A useful pressure state machine would be:

| State              | Response                                                                                                         |
| ------------------ | ---------------------------------------------------------------------------------------------------------------- |
| Normal             | Account resources; allow applications to burst                                                                   |
| Early pressure     | Ask background applications to trim caches; reclaim selected groups; reduce background I/O and CPU priority      |
| Sustained pressure | Apply `memory.high` to the groups creating pressure; page out lower-value background workloads                   |
| Severe pressure    | Request state checkpoints; present a protected overlay identifying the likely offender                           |
| Emergency          | Terminate the lowest-value complete application cgroup while preserving compositor, input, audio and recovery UI |
| Recovery           | Restore priorities gradually and provide an explanation of what happened                                         |

The victim score should not simply select the process with the largest resident memory. It should consider:

* Memory growth rate.
* PSI contribution.
* Reclaim and page-fault activity.
* Swap use.
* I/O stall generation.
* Foreground status.
* Whether the application has unsaved work.
* Whether it supports state restoration.
* User-designated importance.
* Whether it is the workload actually breaching its budget.

`memory.high` is particularly useful because it targets reclaim and throttling at the offending application while giving the external governor time to decide whether to grant more memory or terminate the workload. `memory.oom.group=1` then prevents the usual partial-process wreckage if termination becomes necessary. ([Kernel Documentation][6])

## Protect the interface explicitly

The compositor core, input service, shell recovery overlay, terminal launcher and audio system should receive conservative memory protection plus high CPU and I/O weights. The reserves must be small enough not to starve the rest of the system, but sufficient to guarantee that the user can:

* Move the pointer.
* Switch windows.
* Open the resource overlay.
* Save work.
* Terminate an offending application.
* Reach a terminal.

This is a much better goal than “never kill anything.” Under genuine exhaustion, something must give. The correct goal is to keep the machine operable, explain the choice and sacrifice the least valuable complete workload rather than the entire interactive session.

## Swap policy

I would use real swap on the NVMe drive with a bounded zswap cache rather than relying exclusively on a large RAM-backed swap device. Zswap compresses pages in RAM before writing them to the swap device, trading CPU work for reduced swap I/O and potentially better desktop responsiveness. The disk-backed layer still provides an escape when compressed memory fills. ([Kernel Documentation][7])

## State preservation is necessary

Automatic termination without state handling can lose work. First-party applications should implement a small session-checkpoint protocol:

```text
prepare-for-pressure
checkpoint-now
checkpoint-complete
restore-instance
```

Legacy applications would still be managed, but with less confidence. Editors and terminals could be given stronger protection, while easily recreated applications could be marked disposable.

Freezing an application is not enough: it stops CPU use but does not release its anonymous memory. Checkpoint-and-restart or targeted paging is required for meaningful memory recovery.

## Limits of the solution

This system can make freezes dramatically less common, but it cannot guarantee that the machine will never hang. A kernel deadlock, failing storage device, uninterruptible driver operation or GPU firmware hang can block below the cgroup policy layer. GPU VRAM also lacks a universally reliable, cross-driver equivalent of the CPU memory cgroup controller.

The OS should therefore distinguish:

* Application resource exhaustion.
* Storage stalls.
* GPU reset or driver failure.
* Kernel-level lockup.
* Hardware failure.

Each needs a different recovery path and a different message.

---

# 6. Centralized file-association handling

The existing Linux desktop standards divide responsibility among several mechanisms. Applications advertise supported MIME types in `.desktop` files, while preference and priority are handled externally. Sandboxed applications may use the XDG OpenURI/OpenFile portal, which can ask the user, use a default, or reuse a previous choice. ([Freedesktop.org Specifications][8])

That is enough for interoperability, but it is not a coherent user model.

Create one `associationd` service, probably as part of `appd`, as the authoritative source of truth. It should:

* Import handler declarations from `.desktop` files and application manifests.
* Maintain user defaults in one typed database.
* Export compatible `mimeapps.list` data for existing applications.
* Implement or drive the XDG portal backend.
* Serve both sandboxed and normal applications.
* Explain why a particular handler was selected.
* Keep an audit history of changes.
* Detect stale handlers after applications are uninstalled.

The choice UI should distinguish:

* **Open once.**
* **Always for this MIME type.**
* **Always for this extension.**
* **Always in this project or folder.**
* **Ask when the extension and detected content disagree.**

File extensions must not be trusted blindly. A file called `.pdf` that is actually an executable or unrelated data should not receive the same treatment as a valid PDF.

First-party applications should be required to use this broker. Toolkit integrations for Qt, GTK and libcosmic would cover most other applications. Arbitrary third-party programs can still implement internal behavior and ignore desktop defaults, so absolute enforcement is not possible without sandboxing every application. The realistic target is that the file manager, shell, portals and distribution-provided applications always behave consistently.

---

# 7. Other worthwhile innovations

Starting from a clean policy layer allows several assumptions to be discarded:

| Existing assumption                                   | Better replacement                                         |
| ----------------------------------------------------- | ---------------------------------------------------------- |
| Desktop is a folder painted behind windows            | Searchable Home Surface with drawers and projects          |
| Process ID represents an application                  | Verified application-instance identity and cgroup          |
| Applications may claim any identity                   | Launch-token-verified app identity                         |
| Memory is handled only when OOM occurs                | Continuous pressure monitoring and staged containment      |
| Updates mutate the live system                        | Staged boot deployments with health checks and rollback    |
| Any `.desktop` file may autostart                     | Permissioned capabilities and selected singleton providers |
| Work organization requires hidden workspaces          | Explicit, named and visible project sessions               |
| Settings are scattered private implementation details | Typed settings schema with GUI, CLI and change history     |
| Storage exhaustion is solely the user’s problem       | Reserved recovery space and enforceable cache budgets      |
| A shell crash should destroy the session              | Minimal stable compositor core plus restartable shell      |
| Notifications are transient interruptions             | Searchable event history with causes and actions           |

## Project sessions instead of virtual desktops

A project would be a visible object, not an invisible screen. It could store:

* Files and directories.
* Open applications.
* Window arrangement.
* Terminal working directories.
* Environment variables.
* Browser references.
* Recent commands.
* Resource priority.
* Permissions and mounted locations.

Closing a project would checkpoint its applications where supported. Reopening it would restore them. The user always sees the project name and contents; there is no question about which hidden desktop contains a window.

## System-wide change history

File operations, application installs, default-handler changes, settings changes, autostart choices, driver changes and updates should appear in one event history.

Entries might say:

> Waterfox was terminated at 14:32 after its application group reached 13.4 GiB and caused sustained full memory stalls. Session state was saved. Restore?

or:

> The default application for `application/pdf` changed from Templar Viewer to Waterfox. Undo.

Not every operation can be reversed, but every important system decision can be explained.

## Background-activity permissions

Applications should not gain perpetual background execution merely by installing an autostart file. The application contract should declare whether it needs:

* Login autostart.
* Continuous background activity.
* Network access while hidden.
* Global keyboard shortcuts.
* Screen capture.
* Input monitoring.
* Notifications.
* Removable-media access.

The settings interface would show every currently running background application and why it is running.

## Services should be separate from indicators

Your automatic-timezone logic currently lives in the custom NetworkManager tray. Its provider ordering also had to be corrected because several IP geolocation databases returned Asia/Jakarta rather than Asia/Makassar. 

In the new system, timezone discovery should be a separate `locationd` or `timed-policy` service. The network indicator should only present its state. The service could combine multiple sources, maintain confidence, remember manual corrections and refuse to silently change the timezone when providers disagree.

This is a general rule: **system behavior belongs in services; tray icons and panels are views.**

## Declarative system-provider selection

There should be one selected provider for each exclusive system role:

* Network status UI.
* Notification server.
* File chooser.
* File manager.
* Lock screen.
* Association broker.
* Clipboard manager.
* Power manager.
* Screenshot service.

Installing a second provider would not silently cause both to start. The user or distribution policy would choose one.

## Package-level integration testing

Your LXQt clock workaround documents a package that included the plugin metadata and translations but omitted the actual `libworldclock.so` implementation.  A distribution-specific test should catch this automatically:

* Every declared plugin has its binary.
* Every default panel component can load.
* Every settings page opens.
* Every default application has a valid executable.
* Every MIME handler refers to an installed application.
* Every autostart entry resolves to one selected provider.
* Every portal has a responding backend.

That sort of testing is more valuable to the user than another compiler flag.

---

# Recommended implementation path

## Stage 1: replacement session on Lubuntu 26.04

Use the existing Lubuntu installation only as the development host. Package every component as a Debian package rather than installing binaries manually under `/usr/local` or relying on untracked home-directory state.

The initial session should contain:

* Existing `cosmic-comp`, configured or patched for one workspace.
* A new minimal shell with the Home Surface.
* Your COSMIC Files fork.
* `appd` with canonical app identity and per-launch cgroups.
* An early `resourced`.
* Central file-association UI.
* A protected compositor-level recovery switcher.

At this point, do not build an installer and do not replace the updater.

## Stage 2: own package repository and reproducible image

Move each maintained fork to a proper downstream repository with:

* An upstream tracking branch.
* A distribution patch branch.
* Automated rebase/build checks.
* Source and binary packages.
* Versioned configuration migrations.
* Automated session tests.

Then generate a minimal Ubuntu 26.04-based image containing your session instead of LXQt or GNOME. This is the point where it begins to function as a distinct distribution rather than an installed desktop experiment.

## Stage 3: hardware and update policy

Add:

* Staged kernel and NVIDIA updates.
* Known-good boot deployment.
* Automatic rollback.
* Hybrid-GPU configuration.
* Suspend/resume validation.
* Monitor-layout watchdog.
* Storage reserve.
* Resource-pressure test suite.

## Stage 4: transactional OS deployment

Once the desktop session is reliable, move the system root to either:

* Snapshot-based deployments with bootable rollback, or
* A/B system images.

APT can still be used to construct deployments; it need not be exposed as a live in-place mutation mechanism.

---

# What the first release should not attempt

A disciplined first version should explicitly avoid:

* A custom Linux kernel fork.
* A new package format or package manager.
* A new libc or basic userland.
* Replacement networking, Bluetooth or audio stacks.
* A new display protocol.
* A semantic or object filesystem replacing paths.
* Transparent checkpointing of every arbitrary Linux application.
* Support for every old NVIDIA generation.
* A complete alternative application store.
* Phone, tablet or server editions.
* Extensions and third-party shell plugins.

Those would dilute the project before its defining ideas were proven.

The first release should prove five things:

1. A desktopless Home Surface is better than wallpaper and desktop icons.
2. Windows cannot become lost during monitor or geometry changes.
3. The machine remains interactive under extreme application memory pressure.
4. File opening is predictable and centrally controlled.
5. Kernel and NVIDIA updates either succeed as a pair or roll back as a pair.

---

# Performance should be defined as responsiveness, not package count

“Streamlined” should not mean merely that the ISO is small or that fewer packages appear in a package list. It should mean:

* Negligible idle CPU usage.
* No unrequested background processes.
* Bounded shell and compositor memory.
* Immediate input response under compilation and file-copy load.
* Controlled degradation under memory pressure.
* Fast application launch from warm and cold cache.
* No long login caused by failed services.
* No storage exhaustion caused by unbounded caches.
* Clear attribution of every resource-heavy workload.

A representative test for your machine would simultaneously run:

* Waterfox with a heavy tab set.
* A large Rust compilation.
* A large cross-drive file copy.
* Video playback.
* Deliberate memory pressure.
* An external-monitor connect/disconnect cycle.

The acceptance criterion would not be “nothing gets slower.” It would be that pointer movement, window switching, audio, the recovery overlay and the terminal remain usable, while the system explains which workloads it throttled.

---

# Bottom line

There is enough room to create something meaningfully different. Your complaints point to real architectural gaps, but they are mostly **above the Linux kernel and below individual applications**.

The strongest version of this project would be:

* Ubuntu 26.04 LTS underneath.
* A Smithay/COSMIC-derived single-workspace compositor.
* A new desktopless Home Surface.
* A COSMIC Files-derived client over a real file service.
* A canonical application identity and launch broker.
* A desktop-aware resource governor using cgroup v2 and PSI.
* One authoritative file-association service.
* A tested, transactional NVIDIA/kernel update path.
* A snapshot or A/B rollback system.
* A strict “one provider per system function” policy.

That is a coherent new desktop OS. Rewriting everything from scratch would probably make it less reliable than Lubuntu. Reusing the stable foundation while replacing the fragmented policy layer is the route that could actually make it substantially better.

[1]: https://www.kernel.org/ "https://www.kernel.org/"
[2]: https://lubuntu.me/lubuntu-26-04-lts-released/ "https://lubuntu.me/lubuntu-26-04-lts-released/"
[3]: https://wayland.freedesktop.org/architecture.html "https://wayland.freedesktop.org/architecture.html"
[4]: https://github.com/smithay/smithay "https://github.com/smithay/smithay"
[5]: https://docs.nvidia.com/datacenter/tesla/driver-installation-guide/kernel-modules.html "https://docs.nvidia.com/datacenter/tesla/driver-installation-guide/kernel-modules.html"
[6]: https://docs.kernel.org/admin-guide/cgroup-v2.html "https://docs.kernel.org/admin-guide/cgroup-v2.html"
[7]: https://docs.kernel.org/admin-guide/mm/zswap.html "https://docs.kernel.org/admin-guide/mm/zswap.html"
[8]: https://specifications.freedesktop.org/desktop-entry/0.9.8/mime-types.html "https://specifications.freedesktop.org/desktop-entry/0.9.8/mime-types.html"
