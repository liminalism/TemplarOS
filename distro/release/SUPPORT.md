# Support policy

TemplarOS follows its base: a distribution version built on Ubuntu 26.04 LTS is supported for as long as the
base receives standard security updates (April 2031), with TemplarOS components receiving fixes for the same
window. There is exactly one supported configuration per hardware profile (`../hardware/`); anything that
`templar doctor` flags as a support violation (standalone NVIDIA installer, third-party driver repos, mixed
flavours) is unsupported until reverted.
