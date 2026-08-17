# ADR-0007: Ubuntu 26.04's kernel (7.0) is the base; mainline 7.2 is a transaction candidate

* **Status:** accepted
* **Date:** 2026-08-17
* **Derived from:** `docs/planning/01-initial-shape.md`, `docs/planning/02-subsequent-shape.md`

## Decision
`distro/base.toml` pins `linux-generic` (7.0 GA; HWE later). Mainline 7.2 is never hand-installed on the
primary installation (hybrid Intel/NVIDIA laptop with prior driver conflicts); it is the first candidate for
`lege-hostd`'s kernel/NVIDIA transaction path once the hardware matrix runs. Kernel + LRM + firmware are
version-locked by `legeos-kernel`.

## Consequences
"A distro that runs 7.0 reliably beats one that advertises 7.2 but occasionally boots to a black screen."
