# distro/branding

`os-release` / `lsb-release` (rendered from `../base.toml` [identity]), the getty banner
(`issue`, installed as `/etc/issue` by the ISO customize hook), the plymouth theme (`plymouth/`,
placeholder), logo + icon theme choice (`icons/`, placeholder), default palette. `templar doctor` fails when
`/etc/os-release` does not say `ID=templaros` + `ID_LIKE=…ubuntu…`.
