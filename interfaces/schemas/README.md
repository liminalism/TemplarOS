# JSON schemas

* `app-manifest.schema.json` — the application contract (`templar_types::AppManifest`).
* `provider-capability.schema.json` — an exclusive-role declaration.
* `settings.schema.json` — one typed setting specification.
* `hardware-profile.schema.json` — a supported machine class.
* `app-variant.schema.json` — where one installed copy of an application came from
  (`templar_types::variant::AppProvenance`); the `provenance_json` of `AppIdentity1.ListVariants`.
* `project.schema.json` — a project session (`templar_session_core::projects::Project`); the payload
  of `Projects1.Describe` and `Projects1.ListDetailed`.

Sample instances live in `distro/app-manifests/` and `distro/hardware/profiles/`.
