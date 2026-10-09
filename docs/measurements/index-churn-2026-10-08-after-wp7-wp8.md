# Index churn — 2026-10-08

Owned private bus and `templar-filed`; quiet tree 100000 entries (synthetic), build tree 20000 entries; 120 s of churn: 63 build bursts (315000 files), 1242 download appends, 1250 edits. Peak daemon threads 28. Label: `after-wp7-wp8`.

Browse1 sampled concurrently: p50 0.53 ms, p95 0.68 ms, p99 0.86 ms; 0 errors/timeouts at the 100 ms bound.

| Scope | Search p50 ms | p95 | p99 | First page p95 | Arrays hit rate | Queue p95 ms | Strategies | Errors |
|---|---|---|---|---|---|---|---|---|
| quiet | 3.12 | 989.28 | 1984.00 | 2.20 | 78 % | 0.00 | arrays: 255, fts5-trigram: 34, like-scan: 36 | — |
| build | 2.67 | 806.00 | 1976.61 | 2.32 | 78 % | 0.00 | arrays: 255, fts5-trigram: 34, like-scan: 36 | — |
| all | 3.13 | 1813.43 | 3892.87 | 2.23 | 79 % | 0.00 | arrays: 256, fts5-trigram: 36, like-scan: 33 | — |
