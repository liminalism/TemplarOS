# Package seeds

Plain package lists (one per line, `#` comments), composed by the image build and by the metapackages:

| Seed | Purpose |
|---|---|
| `minimal.seed` | bootable Ubuntu 26.04 base + systemd-boot, btrfs, apt, networking |
| `session.seed` | the LegeOS session packages (compositor, shell, daemons, portal, CLI) |
| `desktop.seed` | default applications (files fork, settings, terminal, browser policy = Flatpak) |
| `hardware.seed` | firmware, Mesa, Vulkan, PipeWire/WirePlumber, NetworkManager, BlueZ, upower |
| `nvidia-open.seed` / `nvidia-proprietary.seed` | exactly one is included, chosen by `lege-hwdetect` |
| `ime.seed` | one input method (fcitx5) — the `input-method` provider role |
| `a11y.seed` | orca, AT-SPI, speech-dispatcher — the AT stack, installed ahead of the shell's AT-SPI bridge, which is **not yet implemented** (`docs/architecture/accessibility.md`) |
| `dev.seed` | build tooling for a self-hosting image |

Excluded on purpose (ADR-0011): `snapd`, `apport`, `whoopsie`, `ubuntu-report`, second desktop stacks.
