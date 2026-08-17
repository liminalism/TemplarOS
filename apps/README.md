# apps/ — first-party applications

* `lege-settings/` — Settings over the typed schema, providers, background apps, associations, history,
  first-run conflicts (skeleton).
* `lege-files/` — the COSMIC Files fork as a client of `lege-filed` (see `FORK.md`; source in
  `forks/cosmic-files`).

Apps link `lege-client` (SDK: launch identity, checkpoint protocol) and ship an app manifest.
