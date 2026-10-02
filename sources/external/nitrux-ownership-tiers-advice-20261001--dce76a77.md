# Nitrux as an architectural reference for TemplarOS — advisor's note, 2026-10-01

Received by the owner from an outside advisor and pasted into the agent session on 2026-10-02.
Abridged, not verbatim: the passages on rejecting systemd removal and Hyprland are shortened, the
stack diagrams around them dropped, and tracking parameters removed from links. Everything kept
is in the advisor's words.

Yes. **Nitrux is more interesting to TemplarOS architecturally than most small distributions are.** It isn't simply Debian with Hyprland and a theme. Nitrux deliberately separates the operating-system image, persistent state, and user applications. Its own documentation describes it as a specialized, opinionated workstation rather than a conventional general-purpose Debian derivative. [Nitrux](https://nxos.org/documentation/troubleshooting/faq/is-nitrux-right-for-me/)

The 7.0 release you linked is current, but I think **kernel 7.2, Hyprland 0.55.4, and being systemd-free are actually the less interesting parts** for TemplarOS.

### The big idea worth studying: the OS is a chassis

Nitrux effectively divides the machine into:

```text
Tier 0 — Operating system
         immutable

Tier 1 — Persistent machine state
         /var/lib

Tier 2 — User
         /home
         applications
         configuration
```

The root filesystem is read-only. A temporary OverlayFS layer allows apparent changes during a session, but they're discarded at reboot unless an administrator deliberately modifies the underlying system. [Nitrux](https://nxos.org/documentation/system-management/administration/nx-overlayroot/)

Even more radically, **Nitrux removes the normal host package manager.** Users don't `apt install firefox` into `/usr`. Applications are installed independently using its AppBox system, Flatpak, or containers. [Nitrux](https://nxos.org/documentation/software-management/)

That's an interesting lesson for TemplarOS.

You've been approaching reliability primarily as:

```text
Ubuntu
  +
carefully controlled Templar packages
  +
candidate deployment
  +
health test
  +
promote / rollback
```

Nitrux asks a more fundamental question:

> Why allow ordinary applications to modify the operating-system environment at all?

For a tightly scoped personal/developer OS like Templar, that's worth considering.

### But I wouldn't copy Nitrux's implementation

Their implementation has significant tradeoffs. Permanent administrative modifications require entering the underlying filesystem using `overlayroot-chroot`; normal changes to the overlay disappear after reboot.

And its recommended installation layout is quite specialized:

```text
ESP       FAT32
/         XFS
/home     F2FS
/var/lib  F2FS
swap
```

Nitrux says its update/recovery tooling depends on this architecture, including XFS for root.

I don't think Templar should copy that.

You already chose **Btrfs**, and Btrfs gives you a cleaner route to the same broad idea:

```text
@system-A       known-good Templar deployment
@system-B       candidate deployment

@home           user
@state          persistent machine state

boot
 ↓
choose system deployment
 ↓
mount selected system read-only-ish
 ↓
mount persistent state separately
```

That fits your existing hostd deployment work much better.

In other words, **take Nitrux's separation principle, not NX Overlayroot.**

### Their application model is even more interesting

NX AppHub isn't merely an AppImage downloader. Nitrux describes it as a local software manufacturing system.

Applications have declarative YAML recipes. AppHub pulls packages from approved Debian/Ubuntu/Devuan/etc. repositories, assembles them locally into AppImage-derived "AppBoxes", and can apply sandboxing. [Nitrux](https://nxos.org/documentation/software-management/nx-apphub/)

That's clever because it combines:

```text
traditional distro repositories
            ↓
    reproducible recipe
            ↓
    self-contained app
            ↓
     sandbox/isolation
            ↓
       user's home
```

instead of:

```text
apt install application
        ↓
modify shared /usr
        ↓
application becomes part of OS
```

There is a **very strong connection to Templar's application-identity system** here.

You already want Templar to understand:

> "This process belongs to Firefox."

and use that knowledge for Taskfield, resourced, associations, etc.

You could eventually define a Templar application as a stronger object:

```text
Application
 ├── identity
 ├── executable
 ├── icon
 ├── permissions
 ├── resource policy
 ├── file associations
 ├── Taskfield behavior
 ├── sandbox policy
 └── update source
```

That would make the application identity **authoritative from installation through execution**.

That's more interesting for Templar than simply adopting Flatpak wholesale.

### It could solve one of your current architectural problems

Remember the issue we discussed with resourced:

A terminal starts:

```text
kitty
 └── bash
      └── cargo
           ├── rustc
           ├── rustc
           └── linker
```

Cgroup inheritance alone can make all of that look like "Kitty."

If Templar owns application/workload creation more explicitly, you can establish identities deliberately:

```text
application: kitty
    interactive terminal

workload: cargo-build-2481
    spawned from kitty
    background compute workload

application: firefox
    interactive GUI

application: blender
    GPU interactive
```

Nitrux's strict separation between **OS and applications** suggests a natural place to formalize this.

That could become genuinely distinctive.

## Another lesson: aggressively define what the distro is *not*

Nitrux's documentation is unusually explicit.

It essentially says:

- Don't expect a mutable root.
- Don't expect `apt`.
- Don't expect systemd.
- Don't expect GNOME/KDE.
- Don't treat it as ordinary Debian.
- Use our supported software pathways instead.

That's good distro engineering.

A problem small Linux distributions frequently have is:

> "It's Ubuntu, except we've changed 40 things, except sometimes Ubuntu documentation works, except sometimes it destroys our assumptions."

Templar eventually needs a similarly clear boundary.

Something like:

```text
Ubuntu is an upstream source and compatibility base.

Templar owns:

    desktop session
    compositor
    shell
    application identity
    file services
    resource policy
    system update/recovery
    interaction model

Ubuntu owns:

    core userspace
    hardware enablement
    libraries
    firmware
    security maintenance
```

And then decide exactly where `apt` sits.

That boundary will become increasingly important as Templar grows.

## One thing I would *not* copy: removing systemd

Nitrux uses OpenRC, and 7.0 has gone quite far with that architecture. Its desktop services include Hyprland plus separate services for displays, lock/idle, clipboard, audio, portals, power management, AppHub and others.

Interesting engineering, but I don't see a corresponding Templar problem that replacing systemd solves.

Systemd gives you dependency management, cgroups, service isolation, journald, startup ordering and failure supervision.

And you're specifically exploiting cgroups.

Replacing it with OpenRC would mean recreating infrastructure without improving the thing users interact with.

**Nitrux had a philosophical reason to do it. Templar currently doesn't.**

## Likewise, don't copy Hyprland

**Don't chase Nitrux where it innovates. Look for concepts that strengthen the layer where Templar innovates.**

## There's also something to learn from their willingness to remove things

Nitrux doesn't ask "What else can our distro include?" It repeatedly asks "What assumptions can we eliminate?" Whether each choice is wise is secondary. **The resulting architecture has a comprehensible model.**

### One specific idea I would steal

I'd formalize **Templar's three layers of ownership**, inspired by Nitrux but adapted to what you've already built:

```text
┌─────────────────────────────────────────┐
│ WORK                                    │
│ projects, documents, app state          │
│ Homegarden understands this             │
└─────────────────────────────────────────┘
                    ↑
┌─────────────────────────────────────────┐
│ APPLICATIONS / WORKLOADS                │
│ identity · resources · permissions      │
│ Taskfield · associations · lifecycle    │
└─────────────────────────────────────────┘
                    ↑
┌─────────────────────────────────────────┐
│ SYSTEM                                  │
│ versioned Templar deployment            │
│ boring · reproducible · recoverable     │
└─────────────────────────────────────────┘
```

Then enforce the boundaries.

Homegarden shouldn't have to know how Ubuntu packages work.

Firefox shouldn't be able to alter the system deployment.

`resourced` shouldn't have to guess whether `rustc` is Firefox.

An OS update shouldn't touch your projects.

And applications shouldn't become part of the operating system merely because you installed them.

**That would tie together several things you've independently built—Homegarden, application identity, resourced, filed, and deployment/recovery—into one architecture rather than five interesting features.**

[Nitrux's architecture documentation](https://nxos.org/documentation/core-concepts/system-architecture/immutable-architecture/) is worth reading directly; it's unusually explicit about the design rationale.
