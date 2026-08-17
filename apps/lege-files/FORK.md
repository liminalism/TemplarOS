# lege-files — the COSMIC Files fork

Upstream: pop-os/cosmic-files. Patches, and only these:
1. Become a client of `org.legeos.Files`: operations go through the journal (progress, cancel, undo).
2. Storage information always visible: containing volume, free/used/total, reserved, read-only/degraded,
   fs type, pending operation size (`Files.Volumes1`), in the sidebar beside every drive.
3. Last-used directories from `lege-filed` (shared with the portal and Home Surface).
4. "Copy canonical path / URI / shell-escaped path / reveal in terminal" as file-service actions.
5. Open-with through `org.legeos.Associations1`; launch through `Launcher1`.
Manifest: `distro/app-manifests/org.legeos.Files.json`.
