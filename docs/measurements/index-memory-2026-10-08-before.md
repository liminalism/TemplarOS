# Index memory — 2026-10-08

Owned private bus and `templar-filed` over a 100000-entry tree. Label: `before-lanes-shards`.

| Phase | RSS before MiB | Peak RSS MiB | Peak PSS MiB | Peak private MiB | Peak threads | ms | Detail |
|---|---|---|---|---|---|---|---|
| a: cold index | 0.0 | 26.3 | 23.7 | 23.7 | 24 | 2513 | 100002 entries indexed |
| b: arrays build | 26.3 | 49.2 | 46.8 | 46.8 | 25 | 304 | answered by arrays |
| c: 2 clients + snapshot + rebuild | 49.5 | 74.3 | 71.7 | 71.6 | 28 | 181 | snapshot 100002 entries, memfd 19070447 bytes |

Snapshot writer and reader, in process on synthetic rows:

| Entries | Payload MiB | Writer peak heap MiB | Reader peak heap MiB | Write ms |
|---|---|---|---|---|
| 100001 | 19.8 | 18.9 | 25.1 | 174 |
| 1000001 | 201.9 | 199.9 | 287.3 | 1861 |
