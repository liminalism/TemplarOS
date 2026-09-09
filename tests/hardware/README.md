The manual half of the hardware matrix (`distro/hardware/test-matrix.md`) is data: one TOML per
check with its steps, expected observations, and applicable profiles.

`templar doctor --hardware` detects the local profile, runs the *automated* probes
(`crates/templar-hwdetect/src/probes.rs` — bounded, read-only, listed in the matrix document), and
reports every case in this directory as **`not-executed`**, with its steps and expectations. It
never performs a reboot, a suspend, a GPU-mode change, a driver change or a cable operation itself,
and it never reports one of these cases as passed.

`not-executed` is deliberately not `skipped`. Skipped means the machine cannot answer the question
(the tool is not installed, the hardware is not present). Not-executed means nobody asked — which is
the honest description of a plan. Executing these cases is the real-hardware phase, done by a human
on the reference laptop; their results are attached as AKR evidence by hand.

`--profile hybrid-intel-nvidia` makes selection deterministic off-device, and `--check <id>` selects
one case or one probe. `services/templar-hostd/tests/hardware_matrix.rs` keeps this directory and
`distro/hardware/profiles/*.toml` from drifting apart: every case must be well-formed, every id
unique, and every profile reference must resolve.
