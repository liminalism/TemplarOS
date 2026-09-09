# Checkpoint protocol

First-party applications implement `templar_client::sdk::CheckpointHandler`; the wire types are
`templar_types::checkpoint::{CheckpointRequest, CheckpointResponse}`.

```
governor ──(Action::TrimCaches)──▶ sessiond ──PrepareForPressure──▶ app  ──Prepared{released}──▶
governor ──(RequestCheckpoint)──▶ sessiond ──CheckpointNow{deadline}─▶ app ──CheckpointComplete{token,unsaved}──▶
                                                                    app ──Refused{reason}──▶ (governor may still kill after the deadline)
project reopen / crash restore   sessiond ──RestoreInstance{previous}──▶ app (with the stored token)
```

Legacy applications are governed with less confidence (no unsaved-work signal, no restore).
Freezing is not enough — it stops CPU but keeps anonymous memory; checkpoint-and-restart or targeted
paging is what recovers memory.
