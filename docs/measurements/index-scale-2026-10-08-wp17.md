# Two-million-entry filesystem scale — 2026-10-08

The current native release `xtask index-bench workloads --allow-two-million`
completed in an owned headless KVM guest. [Exact report](index-scale-2026-10-08-wp17.json)
retains source/binary hashes, environment, all 100k/2M measurements and exact cleanup.

The fixture contains **2,000,000 actual filesystem entries** plus its root.
The initial scan publishes **2,000,001 rows, complete=true**, in **37.07 seconds**.
The arrays contain the same row count and build in **1.580 seconds**.
The 2M database main/WAL/SHM sum is **1,508,302,848 bytes**; individual final
component sizes were not captured. Four accelerated name needles measure p95
below 30 ms across ten samples each. Path needles continue to use SQL.

| Needle | SQL p50 ms | SQL p95 ms | Enabled p50 ms | Enabled p95 ms | Enabled strategy |
|---|---:|---:|---:|---:|---|
| `d` | 17244.60 | 17299.95 | 24.66 | 25.21 | arrays |
| `f1` | 1087.33 | 1093.53 | 18.70 | 18.98 | arrays |
| `txt` | 1588.14 | 1605.39 | 19.58 | 20.46 | arrays |
| `f12.txt` | 238.76 | 248.04 | 12.99 | 13.12 | arrays |
| `d3/d4` | 676.09 | 686.66 | 677.82 | 686.79 | path-scan |

## Environment and limits

The guest has two vCPUs, 4 GiB RAM and an 8 GiB private ext4 disk with `inline_data`
and 2.4 million inodes. Its kernel watch allowance is explicitly 65,536; production
uses its normal half-of-kernel-budget policy. The 100k prelude arms 20,001 directories
with zero refusals. The monitor's Python library is copied into guest tmpfs to
avoid the earlier 9p allocation failure. Read-only exports expose `/usr` and the
owned payload. There is no host GPU, writable host directory or live service bus.

The 2M fixture generates small 28-byte files; ext4 stores these inline. This is a
filesystem scale proof, not reference-home, NTFS, Btrfs, installed consumer or
physical HDMI qualification. The reported snapshot belongs to the 100k prelude,
not a 2M bulk-memory experiment. SQL fallback remains slow, especially the broad
`d` query (17.30 seconds p95). Measurements do not establish reference-day latency.

The complete workload exits zero after 414.96 seconds. Its temporary trees are
removed by the workload, observations are exported, the exact QEMU child is
reaped, and the exact owned disk is removed. Earlier failed and deliberately
stopped attempts remain separate in the [audit](index-qualification-audit-2026-10-08.md).
