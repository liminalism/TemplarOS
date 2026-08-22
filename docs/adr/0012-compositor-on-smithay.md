# ADR-0012: The compositor is built on Smithay, not forked from cosmic-comp

* **Status:** accepted
* **Date:** 2026-08-21
* **Supersedes:** the recommendation in `docs/planning/01-initial-shape.md` §3
* **Amends:** ADR-0005 (forks manifest), ADR-0008 (licence), ADR-0010 (greeter)

## Context

Planning doc 01 §3 offered two routes to a compositor and picked the first:

> 1. Fork `cosmic-comp` and systematically remove or replace its workspace and placement policy.
> 2. Build a smaller Smithay compositor while using `cosmic-comp` as a compatibility and
>    implementation reference.
>
> The first path is more likely to produce a usable system. The second may eventually provide a
> cleaner core, but it has a much larger initial correctness surface.

That was written before `crates/lege-window` existed. The "much larger initial correctness
surface" the second path was charged with — geometry, recovery, stacking, drag across mixed
scales — is precisely what M1 built and property-tested: 2,730 lines, 55 tests plus eight
2000-case property tests, no async runtime, no D-Bus. The premise has changed, so the conclusion
is re-examined.

Measurements, taken from `cosmic-comp` at `81cd5fdb` (67,571 lines of `src/`) and Smithay at the
pinned revision `1ed69cb`:

1. **The layer the fork would keep is saturated with libcosmic.** `src/shell/element/` is 8,446
   lines, of which 7,304 (86%) import `cosmic::` or `iced`: decorations, resize indicators and
   the drag indicator are libcosmic-themed iced widgets. Keeping `element/` means keeping
   `libcosmic`, `iced_tiny_skia`, `iced_graphics` and `cosmic-text` — most of the dependency
   weight the fork was supposed to shed.
2. **`src/shell/focus/` is not a separable layer.** `focus/mod.rs` consists of `impl Shell` and
   `impl Common` blocks filed in another module; `focus/order.rs` holds `&Workspace` and
   `&FloatingLayout` references and encodes workspace-switch animation policy. It cannot survive
   the deletion of the type it is an `impl` of.
3. **`src/shell/grabs/moving.rs` calls into the deleted code** —
   `workspace.floating_layer.drop_window(…)` and `move_element(…)` — so the interactive move grab
   embeds the floating-placement policy at its call site.
4. **The fork boundary would not insulate us.** Of 92 commits in the last twelve months touching
   the paths a fork would delete, 52 (57%) touched a kept path in the same commit. The rebase
   burden lands in the deleted code continuously.
5. **`src/input/`'s keybinding model is an external COSMIC type**
   (`cosmic-settings-config`'s `shortcuts::Action`), so even the input layer is not COSMIC-free.
6. **Licence.** `cosmic-comp` is GPL-3.0-**only**: no or-later clause, no dual licence, no
   contributor relicensing mechanism. Smithay is MIT.

Against that, Smithay already implements every protocol this project's own documents name —
`xdg_shell`, `xdg_decoration`, `xdg_activation`, `xdg_dialog`, `ext_session_lock_v1`,
`ext_foreign_toplevel_list_v1`, `wlr_layer_shell`, `fractional_scale`, `viewporter`,
`presentation_time`, `dmabuf`, `shm`, `pointer_constraints`, `primary_selection`,
`security_context`, `drm_syncobj` — plus a 7,766-line XWayland window-manager role and a
12,297-line DRM/KMS backend with atomic modesetting, plane assignment, direct scanout and VRR.
Its `anvil` example (9,650 lines, MIT) demonstrates both KMS and winit backends, XWayland, output
hotplug, multi-GPU and damage-tracked rendering, and has no workspaces, tiling or window rules —
which is the shape LegeOS wants.

The founding document's own motivation for this project was that maintaining COSMIC forks had
become the problem: "four separately maintained submodules … rebase and detached-submodule
problems". A fork that deletes half of a directory upstream touches weekly is the most expensive
kind of fork, not the cheapest.

## Decision

`lege-comp` is written against Smithay directly and lives at `compositor/lege-comp` as an
ordinary member of this workspace. It is not a fork and has no upstream tracking branch.

* `crates/lege-window` supplies the entire window-management policy;
  `compositor/COMPOSITOR.md` is the binding event-to-call contract for the render-thread loop.
* Two backends behind one trait: **winit** for development, CI and automated tests, and
  **udev/KMS** for hardware. The winit backend is what makes a compositor testable without a
  display, and is not an afterthought.
* `cosmic-comp` may be **read freely** as an implementation reference for driver, XWayland and
  protocol quirks. Consequently `lege-comp` is licensed **GPL-3.0-only**; every other crate in
  the workspace is AGPL-3.0-only (ADR-0013, which replaced ADR-0008's MPL-2.0). GPLv3 §13 and
  AGPLv3 §13 permit combining the two, each part keeping its own licence, so `lege-comp` may link
  `lege-window` and `lege-comp-integration` unchanged.
* No `cosmic-*` crate appears in the dependency graph. `cosmic-comp` and `cosmic-greeter` leave
  `distro/forks/manifest.toml`; the manifest model (ADR-0005) still governs the remaining forks.
* Compositor-drawn chrome uses `pixelkit-raster` and `pixelkit-text`, which have no dependencies
  of their own — chrome that exists for when everything else is broken should not depend on much.

## Consequences

The workspace gains a large dependency tree (DRM, EGL, GBM, libinput, XWayland) and one
GPL-3.0-only crate, which `deny.toml` must now allow. There is no upstream to inherit fixes from:
driver quirks that cosmic-comp has already solved must be re-solved, informed by reading it.

In exchange there is no rebase treadmill, no 17,600 lines of tiling and workspace code shipped
disabled, and the one-workspace invariant is structural rather than configured — `lege-window`
has no workspace type to hide.

`wlr-output-management` and real screencopy capture are not in Smithay and are ours to write;
neither blocks a first session.
