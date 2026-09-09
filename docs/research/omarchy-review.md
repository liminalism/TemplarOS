# Omarchy, studied for what TemplarOS should learn

Method: cloned `basecamp/omarchy` at tag-equivalent `4.0.0.alpha` (this is a substantially
rewritten version — an ISO installer and a Quickshell-based shell, not the older
curl-onto-existing-Arch script most blog posts describe) into scratch and read files directly:
`README.md`, `manual/01,02,04,05,06,07,30,47`, `default/hypr/bindings/*.lua`,
`config/hypr/bindings.lua`, `themes/catppuccin/*`, `install/omarchy-base.packages`,
`install/hardware/*`, `install/config/theme-system.sh`, `migrations/` (listing + one file),
`default/omarchy/omarchy-menu.jsonc`, `bin/omarchy`, `AGENTS.md`. Fetched three external pages
(the omakase-computing philosophy page, and two independent critiques —
abyss.fish/your_dotfiles_are_not_a_distro and stevedylan.dev's distro-philosophy piece) — those
three are **read via WebFetch summary, not the raw source**, flagged below. I did not find or
read a canonical long-form DHH essay dedicated to Omarchy; the philosophy quotes below come from
`manual/01-welcome-to-omarchy.md` itself, which DHH's team maintains as the project's own voice.

## 1. What Omarchy actually is

Arch Linux + Hyprland (tiling Wayland compositor) + Quickshell, installed from a from-scratch ISO
(`manual/02-getting-started.md`): pacstrap-based, full-disk-encryption by default, choice of
whole-disk or free-space (dual-boot) install, and a deferred-setup path for provisioning a machine
for someone else. The shell is **one long-running Quickshell process** that draws the bar, the
root menu, notifications, OSD, and the lock screen (`manual/05-the-top-bar.md`) — themed as a
unit, not a collection of separate status-bar/launcher/notifier binaries. Packages: 227 entries
across `install/omarchy-base.packages` (151) and `install/omarchy-other.packages` (76), plus 431
`bin/omarchy-*` shell scripts wrapping system actions, routed through one `omarchy` CLI dispatcher
that self-documents its command groups from metadata comments (`bin/omarchy`).

Updates run through `omarchy update`, never raw `pacman -Syu` (the tool refuses direct upgrades
and points at the wrapper — `manual/30-updates.md`): it takes a system snapshot, installs from a
pinned Omarchy package repo + an Arch mirror that trails upstream by one month on the stable
channel, runs pending forward-only migrations (`migrations/`, 83 timestamped shell scripts, e.g.
`migrations/<timestamp>.sh` — plain idempotent-ish scripts, no down-migration), and syncs config.
Four channels: stable, RC, edge, dev (the last tracks a live git checkout). Rollback is a Btrfs
snapshot taken automatically before every update, selectable from the Limine bootloader at boot;
it restores `/` but explicitly **not** `/home`, and the manual says plainly that a stale
`~/.config` after a rollback is the user's problem to sort out manually
(`manual/47-system-snapshots.md`).

Theming: 22 themes, each a self-contained directory
(`colors.toml`, `neovim.lua`, `vscode.json`, `icons.theme`, `backgrounds/`, a boot-unlock-screen
image) that reskins terminal, Neovim, btop, Chromium and the entire shell in one action
(`themes/catppuccin/*`, `manual/06-themes.md`). Keybindings: one declarative Lua table per
concern (`default/hypr/bindings/{applications,tiling,media,clipboard,utilities,voxtype}.lua`)
where `o.bind(chord, human_label, command)` is simultaneously the binding *and* the entry shown by
the self-generating cheat sheet (`Super+K` → `omarchy-menu-keybindings`); user overrides live in a
separate, near-empty stub (`config/hypr/bindings.lua`) that unbinds/rebinds without touching
defaults. Every one of those same actions is also a node in a declarative JSONC menu tree
(`default/omarchy/omarchy-menu.jsonc`, 365 lines) reachable by mouse (`Super+Space`, click or
type-to-search), and the top bar documents an explicit left/right/middle-click-and-scroll matrix
per widget (`manual/05`). "Omakase computing" is the stated philosophy: curated, opinionated
defaults instead of "leave it up to you," on the claim that curation doesn't preclude later
substitution (WebFetch summary of the omakase-computing page — not the raw source). The project's
own voice: *"a beautiful system is a motivating system, and productivity has always been
downstream from motivation"* (`manual/01-welcome-to-omarchy.md`).

## 2. What it does genuinely well

- **One source of truth for bindings, doc, and (partly) menu.** The Lua binding table is the
  binding; `Super+K` is generated from the same table, so the cheat sheet cannot drift from
  reality. Compare this to most WMs where dotfiles and a wiki page slowly diverge. Read directly:
  `default/hypr/bindings/*.lua`.
- **Context-scoped bindings that clean up after themselves.** The screenshot-region picker binds
  `RETURN`/`TAB`/arrow keys only while a `selection` layer is open, tracked by a counter and
  unbound on `layer.closed` — a real mechanism, not a manual toggle
  (`default/hypr/bindings/utilities.lua`).
- **Theme atomicity.** One directory, one `colors.toml`, applied by one action, and it reaches
  terminal + editor + browser + compositor + lock screen + notifications together. There is no
  half-themed state to debug.
- **Update safety net that composes two cheap, well-understood primitives**: a Btrfs snapshot
  (coarse, whole-root, free) plus timestamped forward migrations (DHH's own Rails idiom, reused
  here for system config) — not novel engineering, but a pragmatic combination that gives most of
  the safety of a transactional update at a fraction of the implementation cost.
- **Menu-as-data.** `omarchy-menu.jsonc` entries carry `when`/`checked`/`disabled` as shell
  expressions evaluated at render time, so menu rows reflect live system state (installed?
  toggled?) without bespoke per-item UI code (`default/omarchy/omarchy-menu.jsonc` header
  comments + rows).
- **Guardrails around the escape hatch.** `omarchy update` blocks a bare `pacman -Syu`/`yay -Syu`
  and tells you how to override for a single transaction — nudging the safe path without removing
  the power-user one (`manual/30-updates.md`).

## 3. What TemplarOS should steal, ranked

**1. A single action manifest that drives the keyboard cheat sheet, the menu, and the CLI —
not three parallel implementations.**
(a) *Practice:* define every user-facing action once (id, human label, optional chord, CLI verb)
and generate the discoverability surface, the shell's menu, and `templar`'s subcommand table from
it — mirroring Omarchy's `o.bind(chord, label, command)` triple.
(b) *Fit:* TemplarOS already has the underlying discipline for this — M1 evidence states "every
service is reachable from the CLI, the portal, the shell and Settings"
(`docs/generated/CURRENT-STATE.md`, evidence `m1-clients-reachable`), and
`docs/architecture/accessibility.md` already commits to "everything the shell shows is available
through `templar` too." What's missing is Omarchy's fourth leg: a generated, always-current,
**mouse-browsable** list of what a chord does, so a person never has to memorize or hunt a wiki.
This is compositor/shell surface work, so it's M2-adjacent once `templar-comp`/`templar-shell`
keybindings exist, but the manifest itself (which action, which label) can be drafted alongside
the M2 dock/Home Surface work now.
(c) *Cost:* low-medium — a manifest format plus one rendering path in `templar-shell`; no new daemon.
(d) *Conflicts:* the manifest must live in a service crate, not the shell, to respect "system
behaviour belongs in services; ... shell only renders" (`CURRENT-STATE.md`,
`services-not-indicators`) — doable, and actually *reinforces* that constraint rather than
straining it.

**2. Menu/panel rows as data with live `when`/`checked` conditions, not bespoke per-item code.**
(a) *Practice:* declare shell surfaces (dock, panels, root menu equivalent) as data with
render-time predicates against live state, the way `omarchy-menu.jsonc` does.
(b) *Fit:* natural once the Home Surface and dock exist (M2) and grows with Settings1/Providers1 —
each new provider role or setting becomes a data row, not new shell code.
(c) *Cost:* low if designed in from the start of the M2 shell work; retrofitting later is more
expensive.
(d) *Conflicts:* none found; complements the "one typed settings schema" decision already made.

**3. Snapshot-before-mutate as a blanket safety net *underneath* the precise kernel/driver
transaction, for the things that transaction doesn't cover** (settings/state-DB migrations, shell
config changes).
(a) *Practice:* a coarse, cheap, automatic pre-change snapshot (TemplarOS is already on btrfs per
`ARCHITECTURE.md`) as a fallback, alongside `templar-state`'s existing SQLite migrations
(`ARCHITECTURE.md`: "templar-state | sessiond, filed | SQLite migrations").
(b) *Fit:* this is explicitly **not M2** — the five first-release claims don't mention it, and
`@templaros.constraint.first-release-scope` bounds what M1/M2 attempt. It's an M3+ or standing-track
candidate, and belongs behind an ADR, since it brushes against the already-decided, more rigorous
kernel/driver transaction design (D12/ADR-0012 territory) rather than replacing it.
(c) *Cost:* medium — mostly deploy/image policy, not new runtime code.
(d) *Conflicts:* risk of scope creep against the explicit first-release boundary; should be logged
as a track item, not started now.

**4. "Omakase" as validation, not a new work item.** TemplarOS has already independently arrived at,
and formalized more strictly than Omarchy, the opinionated-default philosophy: "exactly one
selected provider per exclusive role may autostart" (`CURRENT-STATE.md`,
`one-provider-per-role`) is omakase computing with an enforcement mechanism instead of a slogan.
No action item — this is a philosophy note confirming a decision already made. Omarchy has no
equivalent mechanism at all: it is a single-vendor stack (one shell, one bar, one notifier) so the
conflicting-provider case it would address simply doesn't arise for its own components, which is
a narrower problem than the one TemplarOS's constraint actually solves.

## 4. Where TemplarOS is already ahead, or an Omarchy choice argues against itself

- **Application identity.** Omarchy/Hyprland resolve window identity from the client's
  self-declared `app_id`/`WM_CLASS` (confirmed by grep: `shell/plugins/bar/widgets/ActiveWindow.qml`,
  `shell/plugins/menu/Menu.qml` both key off `app_id`). That is exactly the spoofable,
  self-declared model TemplarOS's constraint was written to reject: "Identity comes from the launch
  cgroup, never from a client's self-declared app_id" (`CURRENT-STATE.md`,
  `identity-and-launch-safety`). This isn't a style difference — it's a concrete case where
  Omarchy's approach is a real argument *for* TemplarOS's stricter model, because a malicious or
  buggy client can lie about its `app_id` under Omarchy's model and cannot under TemplarOS's.
- **Rollback precision.** Omarchy's update rollback is a whole-root Btrfs snapshot that explicitly
  does not touch `/home` and explicitly leaves config-format mismatches for the user to "sort out
  manually" (`manual/47-system-snapshots.md`). TemplarOS's paired kernel/driver transaction with a
  system-scope health probe and `systemd-bless-boot` gating (`ARCHITECTURE.md`, `templar-boot-health`,
  M1 evidence `m1-update-transaction-resumes`) is a materially more precise mechanism for the one
  case it covers. TemplarOS is ahead here; the honest caveat is that its precision is currently scoped
  to kernel+driver only, and everything else (settings, shell config) has no such story yet — see
  recommendation 3.
- **Memory pressure.** I searched Omarchy's package lists and shell/config trees for any OOM or
  PSI-aware containment (`earlyoom`, `nohang`, `systemd-oomd`, PSI) and found none. Omarchy relies
  on whatever the kernel/systemd do by default. TemplarOS's staged, trace-reviewed governor
  (`templar-resourced`, M1 evidence `m1-governor-staged`) has no analog in Omarchy at all — this is
  claim 3 of TemplarOS's five, and Omarchy simply doesn't compete on it. Not a criticism of Omarchy
  (it isn't trying to solve this), but confirms this differentiator is real and unclaimed.
- **Personal taste baked into system defaults.** Two independent critiques (read via WebFetch
  summary): abyss.fish's *"your dotfiles are not a distro"* argues Omarchy bundles the author's own
  proprietary-service preferences into defaults — and this is directly verifiable in the source I
  read myself: `default/hypr/bindings/applications.lua` binds `Super+Shift+C/E` to
  `https://app.hey.com/calendar` and `https://app.hey.com` (DHH's own company's product), and
  `Super+Shift+A`/`Super+Shift+Alt+A` to ChatGPT/Grok specifically. That's a company's own SaaS
  wired into a distro's hotkeys, not neutral infrastructure. TemplarOS's provider-role model
  (`one-provider-per-role`) is deliberately the opposite shape — a pluggable slot, not a hardcoded
  vendor — and this is a case where Omarchy's opinionation went further than its own "omakase"
  argument requires, and where TemplarOS's more restrained, infrastructure-only stance is the better
  call, not just a different one.
- **stevedylan.dev's critique** (WebFetch summary): heavily opinionated distros become hard to
  customize once a user's needs diverge, because the pieces are bundled rather than composable.
  This is a forward-looking caution for TemplarOS more than a live problem — TemplarOS is pre-GUI (M2),
  so there's no customization ossification yet, but it's worth keeping "one small policy daemon"
  from calcifying into "one way to do everything" as the shell grows past M2.

## 5. The keyboard question

Omarchy's own navigation manual overclaims: *"Everything in Omarchy happens via the keyboard —
EVERYTHING! ... you literally can't do a thing with the mouse alone"* at first boot
(`manual/04-navigation.md`). That's marketing, not the actual design — the top-bar manual, read
minutes later, documents a full left/right/middle-click-and-scroll matrix for every panel widget
and says the panels "aren't read-outs, they're where you actually do the thing" — sliders, lists,
Tab-to-next-panel (`manual/05-the-top-bar.md`). The root menu (`Super+Space`) is fully
mouse-navigable and type-to-search. So the honest description of Omarchy is **keyboard-primary
with full, first-class mouse parity for discovery and occasional use** — not keyboard-only. The
"EVERYTHING" line is the one place the project's own docs contradict its own shipped behavior.

What the keyboard-first model actually buys, mechanistically (not vibes): (1) *no target
acquisition* — a chord fires regardless of cursor position, so window/workspace ops that would
otherwise require aiming at a 16px glyph are instant; (2) *composability* — chords stack
(`Super+Shift+Alt+Arrow` reads as one operation, "move silently to workspace N"), which a menu
tree cannot express as cleanly; (3) *one declarative source doubling as documentation*, per §2/§3
above — that benefit has nothing to do with the keyboard per se, it's a property of *declaring
actions once*, and it is fully available to a menu-and-mouse system too, as Omarchy's own menu
JSONC proves.

Given the user's stated position — not a fan of doing everything by keyboard, while granting the
appeal — the concrete recommendation is: **adopt the "declare once, reach three ways" pattern
(recommendation 1), and make the keyboard layer strictly additive.** Concretely: every action
TemplarOS exposes should have a mouse/menu path that is not second-class (not smaller click targets,
not fewer capabilities, not requiring the shortcut cheat-sheet to discover it exists) — the way
Omarchy's panels genuinely are full UI, not shortcuts-in-disguise. A keyboard chord should be an
*accelerator* recorded in the same manifest as the corresponding menu/dock entry, never the only
way to reach a capability, and TemplarOS should resist adding chords whose only justification is
"faster for someone who already knows them" if that same someone can just as easily click a dock
item that's always in the same place. Given TemplarOS's actual population — accessibility invariants
already require every shell surface to have a high-contrast variant and be reachable without the
shell (`docs/architecture/accessibility.md`) — a mouse/AT-SPI-first design discipline is already a
stated requirement, not a gap; the keyboard-additive stance recommended here is consistent with,
not a change to, that existing invariant. "You may" (chord as accelerator) rather than "you must"
(chord as the only route) is exactly the distinction to hold onto, and it is also exactly where
Omarchy, read carefully rather than taken at its own word, actually lands.

## 6. What I could not determine

- I did not build or run Omarchy; all install/update/rollback claims are read from scripts and
  manuals, not observed behavior.
- I did not find a long-form DHH essay specifically about Omarchy's design philosophy (searched);
  the philosophy statements here come from the manual's own welcome page and a redirected
  omakase-computing doc page, read via WebFetch summary rather than verbatim.
- The two critique pieces (abyss.fish, stevedylan.dev) were read as WebFetch summaries, not
  verbatim source — I independently verified their most load-bearing specific claim (the Hey.com/
  Grok/X keybindings) against the actual source file, but did not verify every claim in either
  piece (e.g., their characterization of the terminal config as "unnecessarily complex").
  Presented as "critiques exist and here's what direct evidence supports," not as fully
  fact-checked op-eds.
- I did not examine the Quickshell QML implementation in depth (`shell/Ui`, `shell/Commons`,
  `shell/services`) beyond the two files grepped for app-identity handling — there may be more
  nuance in how Omarchy's shell handles window identity than the two call sites I read.
- I did not assess actual install/boot performance, hardware compatibility breadth, or real-world
  update reliability — only what the scripts declare they do.

---

## 7. Verification pass, and three corrections

The claims above were spot-checked against the cloned source. The `o.bind(chord, label, action)`
triple is real, and so are the vendor keybindings. Three things need correcting, one of which
changes the headline finding.

**Correction 1 — Omarchy does have provider indirection.** §3.4 says Omarchy "has no equivalent
mechanism at all" to TemplarOS's provider roles. It does. Bindings do not name binaries: they name
capabilities — `o.bind("SUPER + SHIFT + RETURN", "Browser", { omarchy = "browser" })` — and
`bin/omarchy-launch-browser` resolves that through `xdg-settings get default-web-browser`,
falling back to `xdg-mime query default x-scheme-handler/https`. That is the standard XDG
mechanism rather than nothing. TemplarOS's `Providers1` is still meaningfully stricter — one selected
provider per *exclusive* role, arbitrated by a daemon and enforced at autostart, rather than a
per-scheme default anyone can overwrite — but the difference is degree and enforcement, not
presence.

**Correction 2 — the vendor bindings are opt-out, not unconditional.** The Hey.com, Grok and X
chords sit inside `if o.preinstalled_bindings_enabled() then` (`default/hypr/bindings/applications.lua:10`,
`default/hypr/helpers.lua:84`). The criticism that a company wired its own SaaS into a
distribution's hotkeys still stands as a default-values question, and it is a real one; but they
are behind a toggle, and the report reads as though they are not.

**Correction 3 — and this is the interesting one — Omarchy launches applications into transient
systemd units, and then discards that information.**

`bin/omarchy-launch-browser` ends with:

```sh
systemd-run --user --quiet --collect --unit="omarchy-browser-$(date +%s%N)" …
```

So Omarchy independently arrived at the same primitive TemplarOS's launch model is built on: one
transient systemd unit, and therefore one cgroup, per launch. Six scripts under `bin/` do this.

What it does not do is use it. There is **no** reference to a cgroup, a unit, or `/proc/<pid>/cgroup`
anywhere under `shell/` — the shell identifies windows purely by the client's self-declared
`app_id` (12 references). The cgroup exists and is thrown away.

That reframes §4's first bullet. The distance between the two projects is not "Omarchy trusts
`app_id`, TemplarOS uses cgroups" — it is that Omarchy has the cgroup already and cannot get from a
window back to it, because the unit is named after a timestamp (`omarchy-browser-<nanos>`) and
carries no identity. TemplarOS's actual insight is narrower and sharper than the original framing
suggests: it is not that a cgroup exists, it is that
`app-templaros-<AppId>@<InstanceId>.service` **makes the cgroup name carry the identity**, so a
window's PID resolves to a cgroup, the cgroup name resolves to an `AppId`, and the governor,
the dock and the notification router can all key off the same fact without asking the client
anything.

Independent convergence on the primitive is good evidence the primitive is right. The naming
convention is the part that turns it into an identity system, and that is the part worth
defending in review.
