# Search-array layout — 2026-10-08

100001 rows from a synthetic 100000-entry tree; built in 65.3 ms with a peak RSS rise of 7.2 MiB. Columns hold 5.5 MiB in use (57.4 B/row) and 5.5 MiB of capacity. Label: `after-wp11`.

| Column | Bytes in use | Capacity bytes |
|---|---|---|
| by_mtime_desc | 400004 | 400004 |
| by_name | 400004 | 400004 |
| by_size | 400004 | 400004 |
| dirs_by_id | 160008 | 160008 |
| flags | 100001 | 100001 |
| ids | 400004 | 400004 |
| kind | 100001 | 100001 |
| mtime | 800008 | 800008 |
| name_end | 400004 | 400004 |
| name_rank | 400004 | 400004 |
| names | 575004 | 575004 |
| size | 800008 | 800008 |
| subtree_end | 400004 | 400004 |
| subtree_start | 400004 | 400004 |

| Needle | p50 ms | p95 ms |
|---|---|---|
| `d` | 1.24 | 1.41 |
| `f1` | 0.91 | 1.05 |
| `txt` | 1.16 | 1.27 |
| `f12.txt` | 0.61 | 0.70 |
