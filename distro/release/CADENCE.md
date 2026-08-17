# Cadence

* Point releases (`0.x`) as milestones land: M0 scaffold → M1 replacement session on Lubuntu 26.04
  (Stage 1) → M2 own archive + image (Stage 2) → M3 hardware/update policy (Stage 3) → M4 transactional
  deployment (Stage 4).
* Kernel: track Ubuntu's GA kernel; adopt the HWE kernel only through hostd's transaction path with the
  hardware matrix green.
* Forks: rebase on upstream tags monthly (`just forks-fetch`, CI rebase check).
