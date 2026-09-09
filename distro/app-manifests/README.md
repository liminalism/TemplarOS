# Curated application manifests

Installed to `/usr/share/templaros/apps/`. First-party manifests live here until each app crate ships its own;
curated alias manifests for third-party applications (Waterfox, Steam, Wine prefixes) live here permanently
so dock grouping, associations and resource accounting stop needing per-app heuristics.
Schema: `interfaces/schemas/app-manifest.schema.json`. Validate: `just check-manifests` (later).
