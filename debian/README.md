# debian/ — source package `legeos`

Lives at the repo root because `dpkg-buildpackage`, `sbuild` and `gbp` expect it there. Builds the whole
Cargo workspace offline from a vendored `vendor/` directory shipped in the orig tarball (`just vendor`) and
splits it into one binary package per daemon/app (`*.install`). Forks (`lege-comp`, `lege-files`, …) are
separate source packages in their own repositories. Metapackages: `distro/metapackages/` (source package
`legeos-meta`). Quick local packages during Stage 1: `packaging/README.md`.
