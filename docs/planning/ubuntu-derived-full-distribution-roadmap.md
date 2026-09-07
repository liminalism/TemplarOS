# From LegeOS desktop to a supportable Ubuntu-derived distribution

Status: planning proposal, 2026-09-03. This document describes the distribution work beyond the LegeOS
desktop/session itself. It proposes a new kernel policy and therefore does **not** silently alter the
accepted policy in ADR-0007; accepting this plan should be followed by an ADR that supersedes ADR-0007.

## Executive decision

LegeOS should become an **Ubuntu 26.04 LTS binary derivative with a small, owned overlay archive**, not a
source rebuild of all Ubuntu. Ubuntu should continue to provide the base archive and security updates for
glibc, systemd, apt/dpkg, Mesa, NetworkManager, PipeWire, BlueZ, firmware and the thousands of other
packages LegeOS does not need to change. LegeOS should own the integration boundary: its desktop packages,
seeds and metapackages, image and installer configuration, archive signing, release metadata, update and
rollback policy, hardware qualification, and—because the requested kernel is newer than Ubuntu's—the
complete kernel-and-driver release lane.

As of this assessment, kernel.org lists **Linux 7.2.3 as stable** and **7.3-rc1 as mainline**. The practical
policy is therefore:

1. Make the latest final 7.2.x kernel the LegeOS candidate, then default, after it passes defined gates.
2. Carry 7.3 release candidates only in an explicitly unsupported `experimental` channel. Promote 7.3
   only after a final upstream release and the same qualification process.
3. Keep Ubuntu's signed 7.0 kernel as a rescue/fallback entry through the first stable LegeOS release.
4. Build, sign, publish and service the LegeOS kernel as real Debian source/binary packages. Ubuntu's
   mainline `.deb` builds explicitly discourage production use and are suitable only for comparison and
   early hardware testing.

This is feasible, but it substantially increases the permanent maintenance obligation. A bootable VM
image is relatively close. A responsibly installable and updatable OS for a narrow amd64 hardware scope
is not. Roughly **70–85% of the distribution-engineering work remains**, even though much more of the
desktop environment may already exist. A realistic order-of-magnitude estimate from the current tree is
**31–56 experienced engineer-months** for a supportable narrow 1.0, or about 12–20 calendar months for a
focused team of three. A solo effort is more likely 2.5–4 years because installer, kernel, security and
release validation contain serial work and must survive real release cycles. A developer VM image can be
reached much sooner.

## What “a full OS” means here

A distribution is not defined by rewriting every layer. Ubuntu itself integrates Debian and upstream
projects. LegeOS becomes a distribution when it can repeatedly turn declared inputs into an artifact that
a user can install, update, recover and trust, and when someone owns failures at every boundary.

For a deliberately narrow LegeOS 1.0, “full OS” means all of the following are true:

- an amd64 UEFI machine can boot a signed or explicitly documented development image;
- a live environment can install LegeOS onto an empty disk and alongside common existing layouts;
- the installed system has a non-placeholder identity, user, locale, keyboard, timezone and networking;
- package and image contents are reproducible from versioned seeds and a signed archive;
- security updates from Ubuntu and LegeOS can be applied without breaking archive consistency;
- a kernel, driver or deployment failure has a tested previous-version boot path and recovery procedure;
- the default desktop covers ordinary hardware and system integration, not only the compositor session;
- supported hardware, known limitations, release notes and end-of-support dates are published;
- release candidates pass automated VM tests and a real-hardware matrix before promotion;
- the project can issue an urgent kernel or LegeOS security fix and can prove which users received it.

It does **not** require LegeOS to fork coreutils, glibc, systemd, dpkg, apt, OpenSSL, Mesa, PipeWire, every
driver, or the Ubuntu archive. Forking those without a product need would only multiply security work.
The [Ubuntu GitHub organization](https://github.com/ubuntu) is useful source material, but it is not a
single repository containing “Ubuntu the distribution.” Relevant work is divided among Ubuntu and
Canonical GitHub organizations, Launchpad source packages/build infrastructure, Debian and individual
upstreams. The unit to copy is the release process and component contract, not the GitHub organization.

## Recommended ownership boundary

| Layer | Reuse from Ubuntu/upstream | LegeOS must own |
|---|---|---|
| Base userspace | Ubuntu 26.04 release, updates and security pockets | archive pins, compatibility monitoring, package exceptions |
| Core boot userspace | systemd/udev, initramfs-tools initially, cryptsetup, microcode packages | boot layout, configuration, recovery entries and tests |
| Kernel | upstream stable source and selected Ubuntu config/patches | packaging, config, ABI/version, signatures, CVE/stable intake, testing and rollback |
| Graphics/drivers | Mesa and firmware from Ubuntu; NVIDIA upstream/Ubuntu packaging as reference | kernel-matched NVIDIA decision and builds, promotion as one tested set |
| Package management | apt/dpkg and Ubuntu mirrors | signed overlay archive, keyring, pockets, publisher, build records and rollback policy |
| Package selection | Ubuntu package pool and seed conventions | LegeOS seeds, dependency closure, metapackages, supported-set report |
| Image | Ubuntu image tools and live-boot components | deterministic recipe, manifests, provenance, smoke tests and release publication |
| Installer | Subiquity backend and Ubuntu Desktop Provision frontend, preferably themed/configured | LegeOS source definition, storage/deployment layout, branding, QA and installer updates |
| Desktop | Wayland/Xwayland, portals, Flatpak and selected Ubuntu utilities | LegeOS session, compositor, apps, defaults, migrations and integration |
| Updates | unattended-upgrades and apt mechanisms | safe policy, staged promotion, deployment transaction, health check and rollback |
| Security | Ubuntu security fixes for unmodified base packages | LegeOS advisories, overlay/kernel fixes, signing keys, vulnerability triage and response |
| Release operations | upstream inputs and conventions | release cadence, freeze, candidate, mirrors/CDN, checksums, notes, support and telemetry policy |

This boundary lets LegeOS be a recognizable independent OS while retaining the advantage of an LTS base.
It also makes the newest-kernel requirement honest: the project owns only the exceptional layer it chose
to move ahead of Ubuntu, rather than pretending Canonical supports that combination.

## Current repository assessment

The repository contains a strong architecture sketch, but most distro surfaces are not yet production
implementations. The distinction matters when estimating completion.

### Present and useful

- [`distro/base.toml`](../../distro/base.toml) centralizes Ubuntu 26.04/amd64, archive, kernel, image and
  identity choices.
- [`distro/seeds/`](../../distro/seeds/) names minimal, hardware, session, desktop, accessibility, input
  method and NVIDIA selections.
- [`distro/metapackages/`](../../distro/metapackages/) sketches dependency policy and mutual exclusion of
  NVIDIA variants.
- [`debian/`](../../debian/) packages the LegeOS workspace programs as a Debian source package.
- [`distro/image/`](../../distro/image/) describes a mkosi UEFI/btrfs developer disk image.
- [`distro/apt/`](../../distro/apt/) declares an aptly repository, archive keyring and pinning model.
- [`distro/policy/`](../../distro/policy/) begins systemd, sysctl, udev, polkit, D-Bus and AppArmor policy.
- [`distro/hardware/`](../../distro/hardware/) defines an initial Intel/NVIDIA profile and promotion checks.
- [`distro/release/`](../../distro/release/) provides versioning, cadence and support-policy intent.
- the deployment design already aims at named btrfs deployments, boot health and rollback rather than
  unrestricted in-place mutation.

These are valuable because they prevent the image from becoming an unreviewable shell script. They do
not yet prove that the selected package closure resolves, boots, installs or upgrades.

### Scaffold or release blocker

- The mkosi image has never been built in the documented stage because it depends on a working LegeOS
  archive. It contains `RootPassword=legeos` and `Autologin=yes`; these are appropriate only for a dev
  artifact and must never enter a release image.
- [`distro/iso/README.md`](../../distro/iso/README.md) is a placeholder. There is no live filesystem,
  ISO recipe, boot catalog, installer source manifest, generated package manifest or installation test.
- [`installer/README.md`](../../installer/README.md) defers the installer choice. Storage, encryption,
  dual-boot, user creation, failure recovery, logs and OEM/first-boot behavior are not implemented.
- The aptly declaration is not a build farm or publisher. There are no source-only upload gates,
  isolated builds, package pockets, promotion job, mirror verification, signed release workflow or
  disaster-recovery procedure for keys and archive state.
- The seeds are plain package lists. They are concatenated for mkosi, but there is no Germinate-like
  dependency closure, supported-package report, uninstallability check, component/license audit or
  generated metapackage build.
- The kernel metapackage still depends on Ubuntu `linux-generic`; the repository contains no LegeOS
  kernel source package, config, signing pipeline, stable-update intake or build/test job.
- NVIDIA metapackages assume Ubuntu LRM binaries that match Ubuntu's kernel ABI. They cannot satisfy a
  self-built 7.2 kernel without a corresponding module packaging and signing strategy.
- Secure Boot remains undecided. The development image uses systemd-boot with Secure Boot disabled.
- The hardware matrix has one narrow hybrid Intel/NVIDIA profile, and its documented runner does not yet
  prove those checks on hardware.
- AppArmor enforcement is deferred for most LegeOS daemons. The privileged host service begins in
  complain mode, so policy presence is not yet containment evidence.
- Plymouth and icon branding are placeholders. Release policy prose is present, but the operational
  release checklist, security response and published artifacts do not exist.

The current tree is therefore best described as **a desktop implementation plus an early distribution
control plane**, not yet an installable distribution.

## Ubuntu components LegeOS actually needs

### 1. Base archive and package contract

Keep Ubuntu 26.04 `main`, `universe` and the necessary parts of `restricted` as upstream repositories.
LegeOS should not mirror or rebuild them initially. It does need a versioned Deb822 sources policy that
states exactly which Ubuntu suites are enabled (`resolute`, `resolute-updates`, `resolute-security`, and
only controlled use of `resolute-backports`), which keyrings verify them, and how LegeOS packages take
precedence without accidentally replacing unrelated Ubuntu packages.

Create `legeos-base`, `legeos-desktop` and `legeos-supported` package sets. Resolve them in CI against a
captured Ubuntu archive timestamp or snapshot so yesterday's successful image cannot silently become
unbuildable today. Generate and publish the complete source/binary manifest and licenses for each image.
The installer and upgrader must use the same sources definition as the image build.

Ubuntu uses seeds and Germinate to turn human package selection into dependency closure and support
reports. LegeOS may use Germinate directly or implement an equivalently auditable resolver, but its
current line concatenation is insufficient. Adopt conventional `minimal`, `boot`, `standard`, `desktop`,
`ship/live` and `supported` views even if they are generated from the existing split files.

### 2. LegeOS overlay archive and build service

The overlay should publish source packages and resulting amd64/all binaries, not accept opaque local
binaries as the release source of truth. The minimum topology is:

- `incoming` or CI staging for signed source uploads;
- `proposed` for successfully built packages and cross-package testing;
- `release` for image/release inputs;
- `updates` for ordinary stable fixes;
- `security` for embargo-capable or urgent fixes;
- immutable snapshots retained for every released image and deployment rollback.

An isolated builder (sbuild, Launchpad/PPA, or an equivalent clean-room worker) must record source hash,
builder image, compiler/tool versions, build log, binary hashes and tests. A publisher then creates signed
`Sources`, `Packages`, `Release`, `InRelease` and by-hash metadata. Aptly can remain the publisher, but it
does not replace build isolation, review and promotion. At least two people or separately protected
credentials should be required to publish a stable release once the project has users.

The practical early route is a private or public Launchpad PPA for clean source builds plus a LegeOS
publisher that snapshots promoted binaries. If PPA constraints cannot support the kernel/signing model,
use dedicated sbuild workers and preserve the same source-upload discipline.

### 3. Boot, initramfs, storage and recovery

Choose one production boot chain before installer work hardens. The least risky first release is
Canonical-signed shim + GRUB with Ubuntu's signed 7.0 rescue kernel, plus a LegeOS-signed kernel enrolled
through MOK. systemd-boot may remain useful for development and could become production policy later,
but it does not solve firmware trust or signing by itself.

The distribution must explicitly own:

- GPT/UEFI partitioning, minimum sizes and alignment;
- btrfs subvolume names, mount options and quota policy;
- initramfs generation and hooks for btrfs, encryption, keyboard and required storage drivers;
- Intel/AMD microcode installation;
- LUKS2 recovery-key and passphrase flows, with TPM-backed FDE deferred until its threat model and
  recovery story are tested;
- boot entries that identify a deployment and retain at least one known-good kernel/deployment;
- a recovery environment capable of mounting, networking, repairing apt state and selecting rollback;
- behavior when the ESP is full, `/boot` is damaged, NVRAM entries disappear or a health probe times out.

The installer, updater and `lege-hostd` must share one storage/deployment library or schema. Three subtly
different implementations would make rollback untrustworthy.

### 4. Live image and installer

Keep mkosi for fast VM/developer images. For the install medium, first prototype Ubuntu's current desktop
path: [Ubuntu Desktop Provision](https://github.com/canonical/ubuntu-desktop-provision) as the themed UI,
[Subiquity](https://github.com/canonical/subiquity) as the backend, and curtin for target installation.
Subiquity is already the backend for Ubuntu Desktop and multiple Ubuntu flavors, supports autoinstall,
driver selection, encryption and reset partitions, and receives current Ubuntu 26.04 maintenance. That
is a smaller long-term fork burden than writing a Rust installer or adopting a separate storage engine.

Do not fork it immediately. First prove a LegeOS source entry in `casper/install-sources.yaml`, a LegeOS
whitelabel/theme, the exact btrfs layout, package source setup, and post-install handoff. Maintain patches
as a small branch only where configuration hooks are inadequate, and upstream generally useful hooks.

The ISO recipe needs casper/live-boot integration, squashfs creation, UEFI boot files, the installer,
firmware needed in the live session, accessibility, networking, hardware probing and a signed checksum.
`ubuntu-image classic` is useful for preinstalled classical disk images and can consume seeds, but its own
documentation says broader ISO/installer output is future work. Do not assume the current four-line ISO
plan can simply select `ubuntu-image classic`. Investigate Ubuntu's livecd-rootfs/build tooling or use a
small reproducible live-build pipeline while keeping all contents derived from the same seed closure.

Installer acceptance must cover, at minimum: empty disk; EFI Windows dual boot; reinstall preserving a
documented data subvolume if supported; offline install; online security updates; Intel/AMD graphics;
NVIDIA selection; Wi-Fi; LUKS passphrase recovery; interrupted install; low disk space; non-US keyboard;
and an automated QEMU autoinstall on every release candidate.

### 5. Base system and ordinary desktop integration

The existing seeds cover the central session, graphics, audio, Wi-Fi, Bluetooth, firmware and power.
A complete desktop seed still needs deliberate choices for:

- account and group administration, hostname, locale, keyboard, timezone and NTP;
- NetworkManager VPN plugins and captive-portal behavior;
- firewall policy and a supported UI or clear command-line contract;
- printing, printer discovery and scanning;
- camera and media-device plumbing;
- removable media, filesystem helpers and safe eject;
- archive extraction, document/PDF viewing and an image viewer;
- browser delivery and codec/legal policy;
- password/keyring/secret storage;
- software installation and update UI for apt and Flatpak;
- crash collection with an explicit privacy decision (the metapackage currently conflicts with Ubuntu's
  `apport` and `whoopsie`, leaving no replacement);
- backup/restore guidance and user-visible disk health;
- accessibility from the greeter and installer, not only after session startup;
- suspend, hibernate policy, lid/dock behavior, multi-monitor scaling and input devices;
- rescue terminal and diagnostic bundle usable when the graphical session fails.

Every item does not require a new LegeOS application. Prefer proven Ubuntu packages and expose them
through LegeOS settings or documentation. The product work is selecting, integrating and supporting the
behavior.

### 6. Updates, upgrades and deployment transactions

There are three different update problems and the design must not conflate them:

1. Ubuntu base package security/bug-fix updates.
2. LegeOS desktop and service updates.
3. Kernel, bootloader, NVIDIA and deployment updates with boot consequences.

Ordinary packages can begin with apt plus unattended-upgrades under a LegeOS policy. Kernel/boot/driver
sets must go through the named deployment transaction, health probe and automatic/manual rollback path.
The archive promotion system should stage a coherent set before any client sees it; client-side locking
cannot repair a repository that published mutually incompatible versions.

Define power-loss-safe state transitions, disk-space reservation, conffile behavior, migration ordering,
retry/backoff, metered-network policy, notification and support-bundle output. Test upgrades from every
supported released image, not just from current source. A distribution-series upgrade (for example,
26.04 to the eventual next base) is a separate later project; LegeOS 1.0 only needs documented in-series
updates and an explicit promise about whether/when the next base upgrade will exist.

### 7. Security ownership

Ubuntu security updates cover only the unmodified Ubuntu packages and kernel combinations Canonical
supports. LegeOS must monitor and respond for all overlay packages, forks and the custom kernel. That
requires:

- a private security contact and public reporting process;
- inventory/SBOM per image and archive snapshot;
- automated CVE/source-version matching followed by human triage;
- kernel stable/CVE intake with an emergency build and promotion path;
- separate offline root/archive keys and online, tightly scoped CI credentials;
- kernel and module signing keys with rotation, backup and revocation procedures;
- dependency and Rust advisory scanning without treating scanner output as proof of safety;
- enforced sandbox policy for privileged services and review of D-Bus/polkit boundaries;
- published advisories that identify fixed package versions and affected releases.

Promising Ubuntu's April 2031 support window means maintaining LegeOS code and its newest-kernel line for
that period. That is a staffing promise, not a field in `SUPPORT.md`. Until the capacity exists, publish a
shorter LegeOS support window while continuing to consume Ubuntu security updates.

### 8. QA, release and support operations

Build a release dashboard from artifacts, not checklist assertions. Each candidate needs:

- reproducible package/image manifests and verified signatures/checksums;
- package installability and upgrade tests against the exact archive snapshot;
- QEMU boot, install, first boot, update, failed-health rollback and recovery tests;
- desktop/session smoke tests including Wayland, Xwayland, portals, audio, network and file indexing;
- real-hardware results across Intel and AMD CPUs/GPUs, multiple storage/Wi-Fi devices and the explicitly
  supported NVIDIA generations;
- suspend/resume, hotplug, external displays, low disk, offline operation and accessibility checks;
- release notes, known issues, upgrade notes, provenance and support end date.

The first release should remain amd64/UEFI only. Adding arm64, legacy BIOS or broad OEM support is a new
workstream, not an incidental package flag. Similarly, begin with a published small hardware tier:
“tested,” “expected to work,” and “unsupported,” rather than implying all Ubuntu-capable hardware is
qualified with the newest kernel.

## Latest-stable kernel lane

### Channel model

Use names rather than hard-coded version expectations in the client:

| Channel | Contents | Support status | Promotion rule |
|---|---|---|---|
| `rescue` | Ubuntu 26.04 signed GA kernel (currently 7.0) | fallback only | follows Ubuntu security archive |
| `candidate` | newest final upstream stable, currently 7.2.3 | testers | build/tests green, no automatic stable rollout |
| `stable` | last qualified LegeOS kernel/driver/firmware set | supported default | staged hardware and upgrade gates pass |
| `experimental` | mainline release candidates, currently 7.3-rc1 | unsupported | manual opt-in, never an automatic upgrade source |

“Latest kernel” should mean the newest **final upstream stable version that LegeOS has qualified**, not
the newest tag visible on kernel.org. A new upstream release starts in `candidate`; it does not displace
the known-good `stable` set on publication day. Security-critical exceptions still require the same boot
and rollback smoke tests.

### Packaging deliverables

Create separate source packages or generated binary packages equivalent to:

- `linux-legeos` producing versioned image, modules, modules-extra, headers and tools;
- `linux-meta-legeos` tracking the promoted ABI without removing the previous good ABI;
- `linux-signed-legeos` or an equivalent signing stage whose unsigned input is reproducibly identified;
- optional debug symbols in a separate repository;
- matching NVIDIA open/proprietary module packages where legally and technically distributable;
- a kernel-set manifest locking kernel ABI, module build, firmware and userspace driver versions.

Start from upstream stable source, import only understood Ubuntu patches/config differences needed for
Ubuntu 26.04 userspace and hardware, and record the delta. Canonical's custom-kernel guide demonstrates
the Debian package structure but explicitly says its ad-hoc build method is not intended for production;
LegeOS needs an automated clean build, signing and SRU-like verification pipeline.

### NVIDIA is the critical coupling

Ubuntu LRM packages contain modules built for Ubuntu kernel ABIs. They cannot be installed as a generic
module payload for LegeOS 7.2. Prefer NVIDIA's open kernel modules on supported Turing-and-newer hardware,
but still build and sign them for each LegeOS ABI. For older supported hardware, either maintain an
equivalent restricted-module package from redistributable sources/interfaces, allow controlled DKMS with
signing and strong failure handling, or drop that hardware from the newest-kernel support tier. This is a
product decision that must precede claims of broad NVIDIA support.

The safest early 1.0 scope is: Mesa-supported Intel/AMD as the primary tier; NVIDIA open modules on a
small qualified set; Ubuntu 7.0 rescue available; proprietary/legacy NVIDIA explicitly limited until its
kernel lane is proven. “Never DKMS” may remain a goal, but it requires LegeOS to operate the LRM-like build
pipeline itself.

### Promotion gates

A kernel set may enter `stable` only when:

- package ABI, initramfs, Secure Boot/MOK signatures and boot entries are correct;
- clean install and upgrade from both supported prior kernels succeed;
- forced failure selects the prior deployment without manual filesystem surgery;
- root filesystems, encryption, NVMe/SATA/USB, common Wi-Fi, audio and input work;
- Intel, AMD and each supported NVIDIA profile passes graphics, external display and Vulkan checks;
- suspend/resume and power tests pass repeatedly, not once;
- Wayland and Xwayland sessions, portals and LegeOS services pass regression tests;
- there is no unresolved severity-1/2 regression and known lower-severity regressions are published.

7.3 should follow this exact process after 7.3 final. Release candidates can provide early feedback, but
shipping an RC as the default would convert upstream stabilization risk into LegeOS user downtime.

## Delivery roadmap

The phases overlap, but the gates should remain ordered. Dates should be set only after Phase 0 measures
real build/test durations.

### Phase 0 — decisions and measurable baseline (2–4 engineer-weeks)

- Accept this ownership boundary and supersede ADR-0007 with the channel policy.
- Decide the production Secure Boot/bootloader route and installer route.
- Build the existing mkosi image from a temporary signed archive; record every failure rather than adding
  untracked host fixes.
- Generate the current seed dependency closure and identify missing/unavailable Resolute packages.
- Define artifact provenance, supported hardware tier and explicit LegeOS 1.0 non-goals.

Exit: one CI-built dev image boots in QEMU from a versioned archive snapshot; no fixed password artifact
is called a release image.

### Phase 1 — package/archive foundation (3–5 engineer-months)

- Convert all LegeOS components and metapackages into clean source builds.
- Add isolated amd64/all builders, signed publishing, pockets, immutable snapshots and promotion.
- Integrate seeds/Germinate or equivalent closure and generate manifests/license reports.
- Add package installability, upgrade and repository consistency gates.
- Document key handling, archive backup and reproducible release inputs.

Exit: a fresh worker can reproduce all LegeOS packages and a bootable mkosi image from declared sources.

### Phase 2 — live image and installer alpha (4–7 engineer-months)

- Create the live ISO recipe and Subiquity/Desktop Provision integration.
- Implement shared partition/deployment layout, user/locale/network setup and LegeOS archive handoff.
- Cover empty-disk, dual-boot, offline, encrypted and interrupted-install cases.
- Remove all default credentials and add automated QEMU install/first-boot tests.
- Implement recovery entry/environment and collection of useful installer failure logs.

Exit: an external tester can install, boot, update and reinstall an amd64 LegeOS alpha without developer
intervention on at least the declared reference machines.

### Phase 3 — newest-stable kernel and driver lane (5–9 engineer-months)

- Package 7.2.x from upstream stable in clean builders with LegeOS config and ABI policy.
- Implement kernel/module signing and the selected Secure Boot enrollment path.
- Resolve NVIDIA open/proprietary scope and build matching modules per supported ABI.
- Add rescue, candidate, stable and experimental channels plus staged promotion.
- Automate kernel stable/security intake and run the expanded real-hardware matrix.

Exit: 7.2 is the qualified default, 7.0 remains selectable rescue, and a deliberately broken candidate
proves rollback. If these gates fail, ship the Ubuntu kernel rather than label an unqualified 7.2 build
stable.

### Phase 4 — update, recovery and security beta (8–14 engineer-months across workstreams)

- Complete deployment transactions, health blessing, rollback, disk-space handling and migrations.
- Define unattended base updates and coherent boot-critical update sets.
- Enforce sandbox/security policy and establish vulnerability/advisory operations.
- Fill ordinary desktop-integration gaps: printing, scanning, firewall/VPN, secrets, software/update UI,
  crash policy, diagnostics and backup guidance.
- Expand automated and hardware regression coverage and test repeated alpha-to-beta upgrades.

Exit: a beta cohort can daily-drive LegeOS and receive several real update cycles, including a kernel
update and rollback, without repository or manual repair.

### Phase 5 — narrow supportable 1.0 (6–12 engineer-months across workstreams)

- Freeze supported hardware and features; close critical/major defects.
- Conduct release-candidate, upgrade, recovery, accessibility and security exercises.
- Publish signed ISO, manifests, SBOM/provenance, checksums, notes, known issues and support policy.
- Establish mirror/CDN monitoring, incident response, archive/key backup and support triage.
- Run at least one point-release rehearsal from the exact production branch and archive snapshots.

Exit: every “full OS” condition above has durable evidence, and the people maintaining the release can
repeat the process without undocumented workstation state.

## Estimate and remaining-work model

These estimates assume experienced Linux distribution engineers, one amd64 architecture, reuse of Ubuntu
packages, a small supported hardware set, no custom app store backend, and no promise to support every
NVIDIA generation. They include implementation and initial validation but not years of maintenance.

| Workstream | Remaining effort | Why it is not already complete |
|---|---:|---|
| Package/archive/build service | 3–5 engineer-months | policy exists; clean builds, pockets, signing and promotion do not |
| Seed closure and image pipeline | 2–4 engineer-months | lists/config exist; reproducible release artifact and manifests do not |
| Live ISO and installer | 4–7 engineer-months | both are placeholders; storage/error/upgrade coverage is substantial |
| Latest kernel, Secure Boot and drivers | 5–9 engineer-months | new permanent release lane; NVIDIA and signing dominate risk |
| Update/deployment/recovery | 4–7 engineer-months | architecture exists; full boot integration and failure proof remain |
| Base integration and security operations | 4–7 engineer-months | service policy is partial; incident/advisory process is absent |
| QA and hardware qualification | 5–9 engineer-months | one matrix sketch; automation, fleet and repeated cycles remain |
| Remaining desktop/product integration | 2–5 engineer-months | central session exists; ordinary OS facilities/defaults need closure |
| Release, documentation, legal and support | 2–4 engineer-months | policies are stubs rather than operated releases |
| **Total, with modest overlap** | **31–56 engineer-months** | approximately **70–85% of distro engineering remains** |

Useful intermediate expectations:

- reproducible developer VM image: 1–2 engineer-months;
- installable hardware alpha using Ubuntu's GA kernel first: 6–10 cumulative engineer-months;
- beta with own archive, 7.2 lane, recovery and real update cycles: 15–25 cumulative engineer-months;
- narrow, supportable amd64 1.0: 31–56 cumulative engineer-months.

Three people do not reduce 36 engineer-months to exactly 12 calendar months. Installer decisions block
some image tests, kernel signing blocks Secure Boot, and beta confidence requires elapsed update cycles.
For planning, use **12–20 months for three focused experienced engineers**, **18–30 months for two**, or
**2.5–4 years solo**, with scope discipline. If LegeOS must support wide NVIDIA/consumer hardware, TPM
FDE, arm64, OEM factory reset, a graphical software store and seamless next-LTS upgrades at 1.0, add at
least 12–24 engineer-months.

After release, budget roughly **1.5–3 ongoing FTE** as a minimum credible maintenance capacity shared
among kernel/security response, archive/release engineering, desktop fixes, QA and user support. The
latest-stable kernel lane makes that lower bound higher and more time-sensitive than an ordinary Ubuntu
flavor using Canonical's kernel.

## Scope controls and no-go rules

- Do not rebuild the Ubuntu archive merely to call LegeOS independent.
- Do not publish kernel.org or Ubuntu mainline test binaries directly to `stable`.
- Do not make 7.3-rc a default. “7.3 when possible” means after final release plus qualification.
- Do not claim Secure Boot support until the image, installer, kernel and modules all pass it on hardware.
- Do not claim five years of LegeOS support until maintainers and key/archive continuity can sustain it.
- Do not add arm64, legacy BIOS, immutable/A-B storage or TPM FDE to 1.0 without removing equivalent work.
- Do not fork Subiquity/Desktop Provision before a configuration-only prototype shows the missing hooks.
- Do not let the live ISO, mkosi image and installed system acquire independent package lists.
- Do not remove the previous kernel/deployment until the new one has booted and been blessed.
- Do not call an image releasable while it contains fixed credentials, autologin or unenforced privileged
  service policy.

## 1.0 acceptance checklist

The full-distribution milestone is complete only when evidence shows:

- [ ] signed source packages build all LegeOS release binaries in clean, disposable workers;
- [ ] the signed overlay archive has proposed/release/updates/security flow and immutable snapshots;
- [ ] generated seeds, dependency closure, manifests and licenses agree with the ISO and installed system;
- [ ] a checksum-verified ISO boots, installs and updates in automated QEMU tests and on reference hardware;
- [ ] installer paths cover empty disk, dual boot, offline use, encryption and failure recovery;
- [ ] no release artifact contains a fixed password or unintended autologin;
- [ ] the LegeOS 7.2 kernel, matching modules and signatures pass promotion gates;
- [ ] the Ubuntu 7.0 rescue kernel and a prior LegeOS deployment can be selected after forced failure;
- [ ] ordinary system facilities and accessibility work from installer through desktop and recovery;
- [ ] Ubuntu and LegeOS security updates have completed multiple staged real-world update cycles;
- [ ] security reporting, advisories, key rotation/backups and incident ownership are operational;
- [ ] supported/tested/unsupported hardware tiers and NVIDIA limitations are public;
- [ ] release notes, known issues, provenance/SBOM, checksums and support end date ship with the release;
- [ ] a point-release rehearsal reproduces the process from retained source and archive snapshots.

## Immediate next actions

1. Write and accept an ADR superseding ADR-0007: Ubuntu 7.0 becomes rescue; LegeOS latest-qualified-stable
   becomes default; RCs remain experimental.
2. Decide ADR-0004's production boot/Secure Boot path. Prototype shim + GRUB + MOK before investing in a
   LegeOS-signed systemd-boot path.
3. Build the current mkosi configuration against a temporary local archive and turn every missing package
   or post-install failure into a tracked closure item.
4. Replace seed concatenation with a dependency-closure report and generate the metapackages from it.
5. Select Subiquity + Ubuntu Desktop Provision for a time-boxed installer spike; prove a LegeOS source,
   btrfs layout and unattended QEMU install before deciding whether a fork is needed.
6. Create a 7.2.3 kernel packaging spike that outputs versioned Debian packages and boots in QEMU; keep it
   outside the stable archive until signing, module and rollback design is complete.
7. Inventory target hardware and reduce the 1.0 support claim to machines the project can repeatedly test.
8. Re-estimate after the first reproducible image, unattended install and 7.2 kernel build. Those three
   measurements will replace the largest assumptions in this document.

## Primary references consulted

- [Linux kernel releases](https://www.kernel.org/) — current stable/mainline status (7.2.3 and 7.3-rc1
  on 2026-09-03).
- [Ubuntu mainline kernel archive](https://kernel.ubuntu.com/mainline/) — labels the builds for debugging
  and explicitly discourages production use.
- [Ubuntu 26.04 `linux-generic` package](https://packages.ubuntu.com/resolute/linux-generic) — Ubuntu's
  supported GA kernel metapackage/version.
- [Ubuntu kernel stable-release updates](https://canonical-kernel-docs.readthedocs-hosted.com/explanation/stable-release-updates/)
  — the recurring maintenance model LegeOS must approximate for an independent kernel.
- [Ubuntu custom-kernel build guide](https://canonical-kernel-docs.readthedocs-hosted.com/latest/how-to/develop-customise/build-kernel/)
  and [kernel glossary](https://canonical-kernel-docs.readthedocs-hosted.com/latest/reference/glossary/)
  — package outputs and the separate signed/meta package roles.
- [Ubuntu Desktop Provision](https://github.com/canonical/ubuntu-desktop-provision) and
  [Subiquity](https://github.com/canonical/subiquity) — current Ubuntu desktop installer UI/backend.
- [Subiquity autoinstall reference](https://github.com/canonical/subiquity/blob/main/doc/reference/autoinstall-reference.rst)
  — source IDs, drivers, storage and reset-partition behavior.
- [ubuntu-image](https://github.com/canonical/ubuntu-image) and its
  [image-definition reference](https://github.com/canonical/ubuntu-image/blob/main/internal/imagedefinition/README.rst)
  — classical preinstalled image and seed inputs, plus the present ISO limitation.
- [How Ubuntu is made](https://documentation.ubuntu.com/project/how-ubuntu-is-made/) and
  [Germinate](https://documentation.ubuntu.com/project/staging/release-team/germinate/) — archive
  development and seed-based package selection.
- [Ubuntu package publishing](https://documentation.ubuntu.com/launchpad/developer/explanation/ubuntu-package-publishing/)
  and [the archive publisher](https://documentation.ubuntu.com/launchpad/developer/explanation/ubuntu-archive-publisher/)
  — signed source upload, isolated builds, apt indexes and publication concepts.

These sources are architectural references, not a commitment to clone Canonical's private operational
scale. LegeOS needs a smaller system with the same essential guarantees: declared inputs, clean builds,
signed publication, controlled promotion, observable installation, safe updates and owned recovery.
