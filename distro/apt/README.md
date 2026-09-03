# distro/apt — the LegeOS archive

* Layout: **aptly** (`aptly.conf`), suite `legeos-26.04`, components `main` (own packages, forks) and
  `nvidia` (metapackages that pin Ubuntu's LRM). Published under `https://archive.legeos.org/`.
* Signing: one offline archive key; `legeos-archive-keyring` ships the public key under
  `/usr/share/keyrings/` and the deb822 source below.
* Pinning: `preferences.d/legeos-nvidia` rejects third-party driver repositories (graphics-drivers PPA,
  NVIDIA CUDA repo) so the archive stays the **one driver authority**; `preferences.d/legeos-priority`
  prefers the LegeOS archive for packages it carries.
* Stage 1: a local aptly repo fed by `cargo deb` (`packaging/README.md`).

## The local development archive (`/var/lib/legeos/devrepo`)

Stage 1 in practice is not aptly but `lege dev repo` (`cli/lege/src/dev/repo.rs`): a flat, signed,
file:// archive holding whole build sets. Three states, each a one-line marker file at the archive root:

* `current` — the set most recently published; what `lege dev install` installs by default;
* `previous` — the set a rollback goes back to; a republish of the same set never overwrites it;
* `staged` — a set written into the archive whose promotion to `current` did not complete (a publish
  still running, or one that failed). It is **not** the boot-deployment `candidate` of `distro/deploy`:
  an archive state and a boot state must not share a name.

`sets/<id>/manifest.json` records the set (packages, D-Bus signatures, store schemas, build provenance)
and `sets/<id>/debs/` holds hard links to exactly that set's `.deb` files, so an image build or a
provisioner can be pointed at one named set rather than at a pool holding three of them.

`lege dev repo verify [--set <id>] [--json]` re-checks what is on disk now: one id across the set, every
component of the matrix present, every pool file where the manifest says it is, `Packages` still
reproducible from the pool (apt-ftparchive's SHA256 lines are the hash check), `InRelease` and
`Release.gpg` verifying against the archive key, and the previous set still installable — it prints the
`apt-get install --allow-downgrades …` line it would use and does not run it.
