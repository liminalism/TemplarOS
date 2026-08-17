# distro/apt — the LegeOS archive

* Layout: **aptly** (`aptly.conf`), suite `legeos-26.04`, components `main` (own packages, forks) and
  `nvidia` (metapackages that pin Ubuntu's LRM). Published under `https://archive.legeos.org/`.
* Signing: one offline archive key; `legeos-archive-keyring` ships the public key under
  `/usr/share/keyrings/` and the deb822 source below.
* Pinning: `preferences.d/legeos-nvidia` rejects third-party driver repositories (graphics-drivers PPA,
  NVIDIA CUDA repo) so the archive stays the **one driver authority**; `preferences.d/legeos-priority`
  prefers the LegeOS archive for packages it carries.
* Stage 1: a local aptly repo fed by `cargo deb` (`packaging/README.md`).
