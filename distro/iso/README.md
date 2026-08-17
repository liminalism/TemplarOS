# distro/iso — installer ISO (later stage)

mkosi has no ISO output. The installer ISO (Stage 2/3 in the planning docs) will be built with
`ubuntu-image classic` or `live-build` from the same seeds and the installer chosen in `installer/`
(ADR pending). Until then the mkosi disk image is the distributable artefact.
