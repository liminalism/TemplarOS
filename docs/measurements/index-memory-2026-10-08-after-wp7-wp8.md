# Index memory — 2026-10-08

Owned private bus and `templar-filed` over a 100000-entry tree. Label: `after-wp7-wp8`.

| Phase | RSS before MiB | Peak RSS MiB | Peak PSS MiB | Peak private MiB | Peak threads | ms | Detail |
|---|---|---|---|---|---|---|---|
| a: cold index | 0.0 | 23.2 | 20.7 | 20.6 | 28 | 1228 | 100002 entries indexed |
| b: arrays build | 23.2 | 44.1 | 41.4 | 41.4 | 28 | 206 | answered by arrays |
| c: 2 clients + snapshot + rebuild | 44.1 | 59.9 | 57.2 | 57.1 | 28 | 157 | snapshot 100002 entries, memfd 13670231 bytes |

Snapshot writer and reader, in process on synthetic rows:

| Entries | Sealed payload MiB | Writer peak RSS delta MiB | Borrowed reader RSS delta MiB | Mapped client + RawEntry RSS delta MiB | Decoded storage MiB | Write ms |
|---|---|---|---|---|---|---|
| 100001 | 18.1 | 0.8 | 18.1 | 20.3 | 8.8 | 156 |
| 1000001 | 184.7 | 0.0 | 184.7 | 274.0 | 90.4 | 1727 |

RSS deltas use `/proc/self/clear_refs=5` and VmHWM, not an allocation profiler. Synthetic scales reuse one process and allocator; retained pages can reduce a later delta. Reader includes mapped pages; client includes that same mapping plus the final owned paths/vector. Decoded storage counts vector capacity and path lengths, excluding allocator overhead. Unlike the before run, frames include COVERAGE/AGGREGATE for the production tree decoder.

