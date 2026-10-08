# Index churn — 2026-10-08

Owned private bus and `templar-filed`; quiet tree 100000 entries (synthetic), build tree 20000 entries; 120 s of churn: 62 build bursts (310000 files), 1194 download appends, 1230 edits. Peak daemon threads 24. Label: `before-lanes-shards`.

| Scope | Search p50 ms | p95 | p99 | First page p95 | Arrays hit rate | Queue p95 ms | Strategies | Errors |
|---|---|---|---|---|---|---|---|---|
| quiet | 3.32 | 25.37 | 1771.42 | 2.00 | 94 % | 0.00 | arrays: 680, fts5-trigram: 17, like-scan: 24 | — |
| build | 2.87 | 12.87 | 1809.22 | 2.06 | 94 % | 0.00 | arrays: 681, fts5-trigram: 17, like-scan: 23 | — |
| all | 3.43 | 16.42 | 3578.68 | 1.94 | 94 % | 0.00 | arrays: 681, fts5-trigram: 20, like-scan: 20 | — |
