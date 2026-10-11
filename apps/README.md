# apps/ — first-party applications

* `templar-settings/` — native Settings over the typed schema, providers, background apps,
  associations and history. Ten navigation groups include Launcher & Files for saved apps,
  terminal profiles, display aliases and hidden-result restoration, and Appearance & Motion
  for compositor transitions. Tuning and diagnostics live in Advanced; unsupported settings
  are reported through capability state rather than offered as effective controls.
* `templar-files/` — the COSMIC Files fork as a client of `templar-filed` (see `FORK.md`; source in
  `forks/cosmic-files`).

Apps link `templar-client` (SDK: launch identity, checkpoint protocol) and ship an app manifest.
See [the launcher guide](../docs/launcher.md) for the shared discovery and favorite workflow.
