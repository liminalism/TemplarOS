# Search-array layout — 2026-10-08

100001 rows from a synthetic 100000-entry tree; built in 75.0 ms with a peak RSS rise of 11.6 MiB. Columns hold 6.2 MiB in use (65.0 B/row) and 8.0 MiB of capacity. Label: `before-lanes-shards`.

| Column | Bytes in use | Capacity bytes |
|---|---|---|
| ids | 800008 | 1048576 |
| root_slot | 200002 | 262144 |
| names | 575004 | 1048576 |
| name_end | 400004 | 524288 |
| subtree_start | 400004 | 524288 |
| subtree_end | 400004 | 524288 |
| kind | 100001 | 131072 |
| flags | 100001 | 131072 |
| size | 800008 | 1048576 |
| mtime | 800008 | 1048576 |
| dirs_by_id | 320016 | 524288 |
| by_mtime_desc | 400004 | 400004 |
| by_name | 400004 | 400004 |
| name_rank | 400004 | 400004 |
| by_size | 400004 | 400004 |

| Needle | p50 ms | p95 ms |
|---|---|---|
| `d` | 1.30 | 1.49 |
| `f1` | 0.92 | 1.13 |
| `txt` | 1.15 | 1.24 |
| `f12.txt` | 0.69 | 0.80 |
