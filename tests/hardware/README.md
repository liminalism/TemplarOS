The hardware matrix (`distro/hardware/test-matrix.md`) is data: one TOML per check with its steps,
expected observations, and applicable profiles. `lege doctor --hardware` detects the local profile
and prints every applicable non-destructive test plan; `--profile hybrid-intel-nvidia` makes selection
deterministic and `--check suspend-resume` selects one case. The runner never performs reboots,
suspend, driver changes, or cable operations itself. Those executions remain the real-hardware phase,
and their results are attached as AKR evidence.
