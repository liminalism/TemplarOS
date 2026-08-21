# Internationalisation

Fluent (`.ftl`), one directory per crate: `<crate>/i18n/<lang>/<crate>.ftl` (libcosmic convention). Example:
`cli/lege/i18n/en/lege.ftl`. Language packs come from Ubuntu (`language-pack-*` in the seeds); the input
method is one provider (`input-method` role, fcitx5 by default). Accessibility invariants:
`docs/architecture/accessibility.md`.

As of M1, `cli/lege`, `shell/lege-shell` and `apps/lege-settings` each carry an `en` `.ftl` file
reserving their user-facing vocabulary (command labels, drawer names, settings pages). No crate
links a Fluent runtime yet — every string a binary actually prints today is a plain Rust literal,
not loaded from these files. Wiring a loader (and adding the `fluent`/`fluent-bundle` crates to
`[workspace.dependencies]`) is a follow-up; until then, treat a `.ftl` file as the source of truth
for *what* needs translating, not as live data.
