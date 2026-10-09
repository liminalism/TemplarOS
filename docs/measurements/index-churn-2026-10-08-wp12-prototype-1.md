# Index churn — 2026-10-08

Owned private bus and `templar-filed`; quiet tree 100000 entries (synthetic), build tree 20000 entries; 120 s of churn: 62 build bursts (310000 files), 1227 download appends, 1230 edits in the build root. Peak daemon threads 30. Label: `wp12-prototype-1`.

Browse1 sampled concurrently: p50 0.51 ms, p95 0.78 ms, p99 0.91 ms; 0 errors/timeouts at the 100 ms bound.

| Scope | Search p50 ms | p95 | p99 | Next-page fetch p95 | Arrays hit rate | Queue p95 ms | Strategies | Errors |
|---|---|---|---|---|---|---|---|---|
| quiet | 2.63 | 3.26 | 3.60 | 1.89 | 100 % | 0.00 | arrays: 1264, like-scan: 1 | — |
| build | 2.09 | 37.63 | 1085.86 | 1.97 | 93 % | 0.00 | arrays: 448, arrays+overlay: 734, fts5-trigram: 34, like-scan: 49 | — |
| all | 6.22 | 32.79 | 2542.89 | 2.05 | 94 % | 0.00 | arrays+overlay: 1191, fts5-trigram: 34, like-scan: 40 | — |
