# compositor/

* `lege-comp-integration/` — the crate the compositor fork links: `DbusIdentityCache` (async resolution via
  `org.legeos.AppIdentity1`, render-thread-safe lookups), protocol names, and the bridge to `lege-window`.
* `FORK.md` — what the cosmic-comp fork (`forks/cosmic-comp`, pinned in `distro/forks/manifest.toml`) changes.

The compositor binary is packaged as `lege-comp` from the fork repository, not from this workspace.
