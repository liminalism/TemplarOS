# apps/ — first-party applications

* `templar-settings/` — Settings over the typed schema, providers, background apps, associations, history,
  first-run conflicts (skeleton).
* `templar-files/` — the COSMIC Files fork as a client of `templar-filed` (see `FORK.md`; source in
  `forks/cosmic-files`).

Apps link `templar-client` (SDK: launch identity, checkpoint protocol) and ship an app manifest.
