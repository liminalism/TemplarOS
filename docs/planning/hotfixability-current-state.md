# TemplarOS hot-fixability: current state and daily-development plan

**Assessment date:** 2026-08-31  
**Repository snapshot reviewed:** `fe147d81961eb355a6fa1e77f4ffc789903a9b37` plus the working-tree changes present at review time  
**Scope:** planning and assessment only; no product implementation is proposed as already complete

## Executive answer

TemplarOS can already be used as a desktop environment under active development, provided it is installed as an **additional session on a stable Ubuntu installation** and the existing Ubuntu session, display manager, TTY access, and user-data backups remain available as recovery paths.

It is **not yet ready to be the only bare-metal operating system on a machine that must remain recoverable**. The installer is deferred, the installer ISO does not exist, clean-install validation is still open, the Btrfs boot-deployment design has not been proven end to end on hardware, and the Secure Boot/bootloader choice remains a proposed ADR.

The practical target should not be “every byte can change without interruption.” It should be:

1. install an internally compatible package set while the current session continues running;
2. activate each change at the smallest honest boundary—new process, service restart, logout, or reboot;
3. preserve user state across that boundary;
4. test the replacement before crossing the boundary; and
5. retain a known-good route back when the replacement fails.

On that definition, the current architecture is promising but incomplete. The shell and separate services are good hot-fix boundaries. The compositor is deliberately a logout boundary. Kernel, driver, firmware, bootloader, and foundational system-library changes remain reboot boundaries. True in-process hot replacement is neither present nor required for useful day-to-day development.

## What “hot-fixable” should mean here

There are four materially different claims which should not be conflated:

| Level | Meaning | TemplarOS status |
|---|---|---|
| Hot install | Replace packaged files on disk while the machine/session runs | Available for the Stage-1 desktop packages |
| Hot activate | Start using the new files after restarting only the owning process or service | Designed for the shell and most daemons; only partially exercised as a coordinated workflow |
| State-preserving replacement | Resume durable work/state after the process changes | Strong foundations in SQLite migrations and operation/update journals, but backward/rollback compatibility policy is not yet explicit |
| In-process code replacement | Replace native code without restarting its process | Not implemented and not recommended as a near-term goal |

Ubuntu and Debian packaging already allow files to be upgraded on a running system, but a process that has the old executable or shared libraries mapped generally continues running the old code. Installing a package and activating its behavior are therefore separate events. Canonical likewise distinguishes package installation from convergence of the running system: Ubuntu exposes an explicit reboot-required state, and Livepatch can defer some kernel-security reboots but does not eliminate the need to install normal kernel updates and eventually reboot ([Ubuntu Pro reboot-required reference](https://documentation.ubuntu.com/pro-client/en/docs/references/commands/), [Livepatch management](https://documentation.ubuntu.com/pro-client/en/latest/howtoguides/enable_livepatch/)).

## Evidence in the current tree

### 1. Development packages can be installed without ending the live session

[`scripts/reinstall-templaros-desktop.sh`](../../scripts/reinstall-templaros-desktop.sh) builds 13 TemplarOS Debian packages as one selected set, installs them with `dpkg`, repairs dependencies with APT, reloads the user-manager unit cache, and runs the installed `/usr/bin/templar doctor --root /`. It deliberately does not restart the live desktop and tells the developer to log out and back in to activate the new compositor and shell.

This is a sensible safety property: a build or package failure does not immediately destroy the desktop from which it is being repaired. The doctor check is also placed before logout, when recovery is easiest.

Limits:

- The operation is not atomic. `dpkg -i` can leave a partially configured set and APT then repairs dependencies.
- The same package set is reinstalled at workspace version `0.1.0`; package identity does not encode the source commit or provide an obvious downgrade history.
- The script reloads unit definitions but does not restart or health-check each restartable service against the new binaries.
- Stage-1 packages do not yet ship Debian maintainer scripts, as documented in [`docs/testing/README.md`](../testing/README.md).
- A successful doctor run proves installed paths and selected structural/runtime preconditions, not a successful graphical login or a complete daily workflow.

### 2. The process boundaries are mostly favorable

The user-session units deliberately separate the desktop into restart domains:

| Component | Current activation boundary | Assessment |
|---|---|---|
| `templar-shell` | User-service restart | Best current hot-fix target. Its unit uses `Restart=always`; applications remain compositor clients while the decorative Home/shell surface reconnects. |
| `templar-resourced` | User-service restart | Designed to restart independently with persistent trace/stage state and `Restart=always`. A controlled replacement still needs a behavioral health gate. |
| `templar-sessiond` | User-service restart | Durable state uses append-only SQLite migrations. Restart interrupts D-Bus availability and may affect clients, so reconnect behavior must be tested before calling it seamless. |
| `templar-filed` | User-service restart | Resumes journalled file operations before claiming its D-Bus name. This is a strong state-preserving boundary, but active clients and in-flight operations need an upgrade-specific test. |
| `templar-hostd` | System-service restart | Update transactions are journalled and resume on startup. Replacing the updater while it owns a transaction is higher risk and should be forbidden or explicitly tested. |
| `templar-settings`, `templar-files`, CLI, ordinary apps | Restart that application / launch a new process | Straightforward; already-running instances retain old code until closed. |
| Portal backend | Service restart, then client reconnection | Architecturally independent, but portal calls in flight may fail and need retry semantics. |
| `templar-comp` | Logout and new login | Intentionally `Restart=no`. Restarting a Wayland compositor disconnects every client; pretending this is seamless would lose application sessions. |
| Greeter/display-manager integration | End the affected greeter/session | Risky during daily work. Keep the host display manager unchanged until this path is independently proven. |
| Kernel, NVIDIA module/userspace set, firmware, boot chain | Staged reboot | Correctly treated as a deployment transition, not a live service restart. |

The compositor exception is a sound design choice, not a hot-fixability defect. During compositor work, [`scripts/nested-session.sh`](../../scripts/nested-session.sh) is the appropriate rapid iteration boundary; the installed hardware compositor changes only after logout.

### 3. Durable-state foundations are better than the activation workflow

[`crates/templar-state`](../../crates/templar-state) opens one SQLite store in WAL mode and applies named, append-only migrations transactionally. `templar-filed` journals accepted operations, and `templar-hostd` journals update phases before moving its in-memory state. These are exactly the kinds of foundations that allow a daemon to be replaced without treating all of its state as process-local.

Two important policies are still missing from the evidence reviewed:

- **Rollback compatibility.** Forward migrations are defined, but there is no stated rule that an older binary can read state after a newer binary has migrated it. A package rollback and a data rollback are not the same operation.
- **Cross-version IPC compatibility.** `interfaces/` is treated as the source of truth and the contributor guidance says shipped interfaces are frozen and incompatible changes require a version bump. That is the right rule, but rolling/skew tests—old client/new service and new client/old service—are not part of the current reinstall gate.

The Home Field document is stricter still: `templar-field` refuses a document with a different `DOCUMENT_VERSION`. That prevents silent corruption, but a future format change needs an explicit migration or recovery experience before the shell can be safely rolled forward and back.

### 4. Whole-system transactional updating exists as an implementation, not yet as a proven product path

`templar-hostd` contains a resumable phase machine:

`Planned → Downloading → Staging → Verifying → AwaitingReboot → Trial → Promoted/RolledBack`

It snapshots the running Btrfs root, installs packages into a candidate rather than the live root, verifies selected kernel/NVIDIA invariants, seals the snapshot, copies boot artifacts into a deployment-owned ESP directory, and stages a boot with a retry counter. `templar-boot-health` gates promotion. The code also accepts `other_packages`, so the mechanism is not structurally limited to kernels.

This is the correct direction for high-risk updates, but current evidence does not justify relying on it as the sole recovery system:

- [`docs/adr/0004-boot-deploy-secure-boot.md`](../adr/0004-boot-deploy-secure-boot.md) is still **proposed** and leaves the production boot/Secure Boot choice open.
- [`installer/README.md`](../../installer/README.md) says installer selection is deferred.
- [`distro/iso/README.md`](../../distro/iso/README.md) says the installer ISO is a later stage; the current mkosi artifact is a development disk image.
- The hardware kernel/driver rollback files are declarative scenarios, and the AKR desktop-readiness assessment records that the documented hardware runner was not actually executing that matrix.
- The clean-install acceptance check under desktop validation is still open.
- Verification currently includes useful structural proxies, but the update code itself notes that module-signature verification is not yet cryptographic.

Treat this path as a development target to validate in disposable VM/disk images before it becomes the foundation of a daily-driver installation.

### 5. Archive/reproducible packaging is not finished

The fast `cargo-deb` path has built successfully in prior recorded checks, but the Stage-2 archive path has two current blockers:

- [`Cargo.toml`](../../Cargo.toml) requires Rust 1.97 and CI uses 1.97.1, while [`debian/control`](../../debian/control) and [`packaging/README.md`](../../packaging/README.md) still claim/build-depend on Rust 1.93.
- The workspace explicitly retains sibling path dependencies which must become pinned dependencies before the vendored source package can be built in isolation.

This does not prevent local iteration from this checkout. It does prevent claiming that a clean TemplarOS archive can currently reproduce and distribute every update.

## Review of the supplied paper

The reviewed paper is *A Programming Paradigm for Spatiotemporal Composability* by Yifan Shi, Wei Zhang, and Tianyi Cui (`/home/dk/Downloads/paper.pdf`, 88 pages). It presents Cordis, a TypeScript meta-framework for dynamically loading and unloading application components.

Its two core ideas are useful vocabulary for TemplarOS:

- **Temporal composability:** every component effect is paired with and tracked alongside an inverse, so removing the component recovers its contribution.
- **Spatial composability:** components declare dependencies (“coeffects”), and the runtime activates, deactivates, or reloads dependents as providers appear, disappear, or change.

Cordis builds hot module replacement from those properties: classify affected modules, identify stale component entries, dispose their old fibers, reload them, and restore cached old modules if import fails. The paper also explicitly recognizes the operating-system process boundary as a coarse-grained but practical form of temporal composability.

### What TemplarOS should adopt from it

1. **Make every activation describe its inverse or recovery route.** For TemplarOS that usually means stop/restart a unit, restore a package/deployment, replay a journal, or restore a compatible state backup—not a closure inside one process.
2. **Declare compatibility and dependencies.** Debian package dependencies, systemd ordering/readiness, D-Bus/Wayland protocol versions, schema versions, and provider roles are the OS-scale equivalent of the paper's coeffects.
3. **Withdraw dependents before providers.** A service replacement should stop accepting new work, let dependent/in-flight work drain or fail retryably, replace the provider, verify readiness, then reconnect consumers.
4. **Reconcile desired state.** A recorded release manifest should name the exact package/build versions expected; the update system should converge to it rather than run an unrecorded series of `dpkg` operations.
5. **Respect the system boundary.** The paper notes that external emissions—file writes observed by others, network messages, and similar effects—cannot simply be inverted. TemplarOS should use transactions, snapshots, delayed commit, or explicit compensation where correctness requires recovery.

### What TemplarOS should not adopt yet

TemplarOS should not turn the compositor, shell, or policy daemons into native plugin hosts merely to obtain in-process HMR. Rust dynamic-library replacement introduces ABI, ownership, thread, callback, and teardown hazards precisely where desktop recovery must remain simple. The project already has a stronger and easier-to-audit granularity: processes supervised by systemd and connected through versioned protocols.

The paper's technique becomes attractive only for a deliberately designed, sandboxed extension runtime whose capabilities and effects are mediated from its first version. It is not a shortcut for replacing arbitrary native code in a running compositor.

## Recommended daily-development operating model now

Use three layers, in this order:

### Layer A — stable Ubuntu host plus alternate TemplarOS session (recommended now)

1. Keep the existing Ubuntu desktop and display manager installed and working.
2. Install TemplarOS as an additional Wayland session with `scripts/reinstall-templaros-desktop.sh`.
3. Do not switch the host to greetd as part of ordinary TemplarOS iteration.
4. Keep a tested TTY login and enough free disk space for builds, package artifacts, and backups.
5. Keep user data outside experiments with filesystem/deployment layout, and maintain an external backup. OS rollback is not user-data rollback.
6. Before each installed-session test, run the nested session and the relevant package/tests; then install, run the installed doctor, and only then log out into TemplarOS.
7. If TemplarOS fails, return through the unchanged Ubuntu session or TTY. Do not repair a broken compositor from inside the compositor session if an independent session is available.

This lets TemplarOS be used and improved on the same computer without asking the unfinished TemplarOS boot/install stack to protect the machine.

### Layer B — selective live activation inside a TemplarOS session (usable with discipline)

Use a component matrix rather than a blanket “restart everything” command:

- Shell-only change: package install, then restart `templar-shell`; verify Home, window controls, launcher, and a pre-existing application.
- Application/CLI change: start a new instance; close old instances when convenient.
- `sessiond`, `filed`, `resourced`, portal, or `hostd` change: first add/run an upgrade-specific restart test, including old/new client skew and durable-state recovery; only then permit a targeted service restart.
- Compositor/protocol change: test nested, install, run doctor, then log out/in. If a protocol changes incompatibly, update compositor and clients as one activation set.
- Kernel/driver/firmware/boot change: do not activate live; use a tested candidate deployment and reboot, or stay with Ubuntu's normal known-good kernel workflow until TemplarOS deployment rollback is hardware-proven.

“Installed successfully” must not be reported as “running successfully.” The workflow should name both the installed build and the active build for each long-lived process.

### Layer C — full TemplarOS installation (not recommended as the sole daily driver yet)

Advance to this layer only after the installer, image, clean-install, boot-health, and rollback acceptance gates below are demonstrated on the target hardware. Until then, the mkosi disk image is suitable for VM/disposable-disk testing, not evidence that a sole-host installation can always recover.

## Planned work, in priority order

### P0 — define and expose activation boundaries

- Add a release/build identity containing package version plus source commit; expose installed and running identities in `templar doctor`/status.
- Maintain a machine-readable component matrix: package, owning unit/process, state path/schema, IPC versions, activation action, interruption promise, rollback action, and health check.
- Make the reinstall workflow produce a final status of: installed set, still-running old processes, safe targeted restarts, logout required, and reboot required.

**Acceptance:** after installing an update, one command can state exactly which components are new on disk, which old binaries are still active, and the minimum action required to converge.

### P0 — make the desktop package set transactional enough for development

- Build all packages first and reject mixed/missing artifacts before elevation.
- Give each build a monotonically comparable Debian version and retain at least one known-good local bundle.
- Prefer the existing local APT repository design over repeated unversioned direct `dpkg` installs, so dependency solving, provenance, and downgrade are explicit.
- Add pre-activation and post-activation health gates; if activation fails, reinstall the retained bundle before ending the rescue session.

**Acceptance:** deliberately break one package, observe that the set is rejected or fails health checks before logout, then restore the previous bundle without rebuilding it.

### P0 — prove restartability rather than infer it from unit files

For each restartable service, run a composed-session test which starts real clients, begins representative work, replaces/restarts the service, and checks reconnection and durable state. Include `filed` during a journalled operation and `sessiond` with shell/settings clients connected.

Define an explicit compatibility window for every D-Bus/Wayland/JSON interface and every persisted schema. A practical starting rule is:

- protocol additions remain backward compatible within a development release;
- incompatible protocol changes use a new interface version and a coordinated activation set;
- migrations are forward-only unless a tested downgrade adapter or state snapshot accompanies the package rollback; and
- destructive schema changes are forbidden in the daily-driver channel.

**Acceptance:** old-client/new-service and new-client/old-service tests pass for the supported skew window, and rollback behavior after every migration is documented and exercised.

### P1 — create a safe compositor activation ceremony

- Keep `Restart=no`.
- Make nested-session validation exercise the same package artifacts that will be installed.
- Before logout, verify unit binaries, XWayland, protocols, assets, and a headless/nested render journey.
- Preserve the alternative Ubuntu session and document the exact TTY rollback command/bundle.

**Acceptance:** a deliberately broken new compositor never prevents returning to the host desktop, and the previous package bundle can be selected/restored from a TTY.

### P1 — finish and prove boot-deployment rollback

- Resolve ADR-0004's bootloader/Secure Boot decision.
- Build the installer around the actual Btrfs subvolume/deployment layout.
- Replace structural signature proxies with the intended cryptographic/module/keyring checks.
- Connect the hardware matrix to a runner and prove promotion and automatic rollback on each supported graphics profile.
- Test power loss/process kill at every transaction phase.
- Decide whether ordinary userspace updates use boot deployments or a separate package-level rollback channel; do not leave two overlapping authorities ambiguous.

**Acceptance:** on real supported hardware, a broken kernel/NVIDIA candidate automatically returns to the previous complete deployment, records the reason, and leaves user data intact.

### P1 — finish reproducible archive and clean-install gates

- Align `debian/control` and packaging documentation with Rust 1.97.
- Replace all sibling path dependencies with pinned, vendorable sources.
- Build the source package in a clean Ubuntu 26.04 environment and install every desktop package into a clean root/VM.
- Complete the open clean-install, greeter-login, recovery-without-shell, and composed-session gates in AKR.

**Acceptance:** the published source plus declared archive inputs reproduce the package set and boot a fresh image without relying on the developer checkout.

### P2 — evaluate finer-grained replacement only where interruption is genuinely costly

After process-level replacement is proven, consider blue/green service instances behind a stable broker for services with expensive in-flight work. Consider a Cordis-like component model only for a future sandboxed extension subsystem. Do not apply this complexity to the compositor or ordinary desktop daemons by default.

## Go/no-go thresholds

| Use case | Current verdict | Minimum next evidence |
|---|---|---|
| Develop TemplarOS from a normal Ubuntu session | **Go** | Existing build/test workflow |
| Install TemplarOS as an alternate session and use it experimentally | **Go with recovery discipline** | Installed doctor passes; Ubuntu session and TTY retained |
| Restart only the shell to try shell/UI work | **Go, but verify the current session first** | Short composed-session restart smoke |
| Routinely restart all TemplarOS daemons after each build | **No-go today** | Per-service reconnection/state/skew tests |
| Activate compositor changes without logout | **No-go by design** | None planned; logout remains the honest boundary |
| Use the current full image as the sole production/daily-driver installation | **No-go today** | Installer + clean-install + real-hardware rollback evidence |
| Update kernel/NVIDIA through TemplarOS and rely on automatic rollback | **No-go as the only safety net today** | Hardware-executed promotion/rollback matrix and resolved ADR-0004 |
| Add native in-process HMR broadly | **No-go / unnecessary** | A narrowly scoped use case that process restart cannot satisfy |

## Bottom line

The user goal—install TemplarOS and continue improving it while using the same computer—is realistic now in the **alternate-session** form. Most iteration can happen from Ubuntu or a nested TemplarOS session; packaged changes can be installed while the old session runs; shell and application changes can use small restart boundaries; compositor work costs a logout; kernel/driver work costs a reboot.

What is not yet justified is removing the stable Ubuntu recovery layer. The next hot-fixability work should therefore focus on observable build identity, compatible service restarts, retained package bundles, and executable clean-install/rollback gates. Those provide far more practical safety than attempting fine-grained native HMR.

