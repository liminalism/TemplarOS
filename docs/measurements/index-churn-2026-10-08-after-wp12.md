# Index churn — 2026-10-08

Owned private bus and `templar-filed`; quiet tree 100000 entries (synthetic), build tree 20000 entries; 120 s of churn: 63 build bursts (315000 files), 1251 download appends, 1260 edits in the build root. Peak daemon threads 30. Label: `after-wp12`.

Browse1 sampled concurrently: p50 0.52 ms, p95 0.73 ms, p99 0.88 ms; 0 errors/timeouts at the 100 ms bound.

| Scope | Search p50 ms | p95 | p99 | Next-page fetch p95 | Arrays hit rate | Queue p95 ms | Strategies | Errors |
|---|---|---|---|---|---|---|---|---|
| quiet | 2.68 | 3.28 | 3.61 | 1.86 | 100 % | 0.00 | arrays: 984, like-scan: 1 | — |
| build | 2.45 | 49.43 | 1209.34 | 1.93 | 90 % | 0.00 | arrays: 232, arrays+overlay: 656, fts5-trigram: 44, like-scan: 53 | — |
| all | 7.17 | 72.41 | 2671.11 | 2.00 | 91 % | 0.00 | arrays+overlay: 895, fts5-trigram: 43, like-scan: 47 | — |
