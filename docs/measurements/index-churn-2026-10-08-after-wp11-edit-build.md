# Index churn — 2026-10-08

Owned private bus and `templar-filed`; quiet tree 100000 entries (synthetic), build tree 20000 entries; 120 s of churn: 63 build bursts (315000 files), 1248 download appends, 1250 edits in the build root. Peak daemon threads 30. Label: `after-wp11-edit-build`.

Browse1 sampled concurrently: p50 0.51 ms, p95 0.78 ms, p99 0.90 ms; 0 errors/timeouts at the 100 ms bound.

| Scope | Search p50 ms | p95 | p99 | Next-page fetch p95 | Arrays hit rate | Queue p95 ms | Strategies | Errors |
|---|---|---|---|---|---|---|---|---|
| quiet | 2.88 | 3.55 | 4.41 | 1.92 | 99 % | 0.00 | arrays: 128, like-scan: 1 | — |
| build | 43.66 | 1641.50 | 1961.75 | 2.22 | 22 % | 0.00 | arrays: 29, fts5-trigram: 45, like-scan: 55 | — |
| all | 93.38 | 3279.27 | 3852.93 | 2.21 | 0 % | 0.00 | fts5-trigram: 64, like-scan: 65 | — |
