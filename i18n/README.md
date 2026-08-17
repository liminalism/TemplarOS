# Internationalisation

Fluent (`.ftl`), one directory per crate: `<crate>/i18n/<lang>/<crate>.ftl` (libcosmic convention). Example:
`cli/lege/i18n/en/lege.ftl`. Language packs come from Ubuntu (`language-pack-*` in the seeds); the input
method is one provider (`input-method` role, fcitx5 by default). Accessibility invariants:
`docs/architecture/accessibility.md`.
