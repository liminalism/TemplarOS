# Index churn — 2026-10-08

Owned private bus and `templar-filed`; quiet tree 100000 entries (synthetic), build tree 20000 entries; 120 s of churn: 62 build bursts (310000 files), 1228 download appends, 1230 edits in the build root. Peak daemon threads 29. Label: `before-wp11-edit-build`.

Browse1 sampled concurrently: p50 0.51 ms, p95 0.70 ms, p99 0.85 ms; 0 errors/timeouts at the 100 ms bound.

| Scope | Search p50 ms | p95 | p99 | Next-page fetch p95 | Arrays hit rate | Queue p95 ms | Strategies | Errors |
|---|---|---|---|---|---|---|---|---|
| quiet | 2.45 | 115.29 | 1601.33 | 1.91 | 82 % | 0.00 | arrays: 367, fts5-trigram: 39, like-scan: 43 | — |
| build | 2.17 | 67.07 | 1634.21 | 2.05 | 81 % | 0.00 | arrays: 365, fts5-trigram: 40, like-scan: 44 | — |
| all | 2.51 | 1302.22 | 3433.15 | 1.98 | 82 % | 0.00 | arrays: 366, fts5-trigram: 40, like-scan: 43 | — |
