# templar-fanotify

fanotify bindings and a `FanotifySource` for the file index, **measured and not adopted**.

The full comparison against inotify — arm cost, kernel and userspace memory, throughput, rename
fidelity, event merging, queue overflow and the Btrfs `EXDEV` question — is in
[`docs/measurements/fanotify-2026-09-04.md`](../../docs/measurements/fanotify-2026-09-04.md).

**The conclusion, so nobody has to re-derive it:** unprivileged fanotify is one mark per directory,
buys 6.4% more headroom against the kernel's ceiling where a home root needs 51% more, arms 2.5x
slower and costs about twice the memory per directory. `FAN_MARK_FILESYSTEM` — the one variant that
would have removed the per-directory cost — needs `CAP_SYS_ADMIN` and returns `EXDEV` on a Btrfs
subvolume, which is all TemplarOS's declared layout contains. A fanotify group cannot even hold marks
on two Btrfs subvolumes at once. `InotifySource` stays the default.

## What is here

| file | what it is |
|---|---|
| `src/sys.rs` | the six syscalls, and **every** `unsafe` block in the crate |
| `src/event.rs` | decoding the packed event stream and its fid info records — safe code over `&[u8]` |
| `src/group.rs` | one fanotify group: flags, mask, marks, limits |
| `src/source.rs` | `FanotifySource`, behind `templar_file_index::watch::EventSource` |
| `examples/fanotify-lab.rs` | the measurement harness |

Nothing in `services/` depends on this crate. It exists so the report can be re-run and the
conclusion revisited if the kernel changes.

## The `unsafe` exception

`CLAUDE.md` asks for `#![forbid(unsafe_code)]` unless a crate documents why not. This one does not
forbid it, and the reason is that no crate in the registry does the job: `rustix` does not
implement fanotify at all, and `nix`'s binding stops at the fixed-size event header — which, in the
`FAN_REPORT_DFID_NAME` mode this is the only usable one for an unprivileged listener, carries none
of the information. The information is in the variable-length info records after it.

So the exception is scoped rather than avoided: `src/sys.rs` holds six foreign calls, each a single
syscall with a `SAFETY` note, and nothing else. Every decision with room for error — parsing a
length-delimited stream, keying a directory by file handle, reconstructing a rename, resolving a
merged mask — is safe code with unit tests against hand-encoded kernel layouts. `templar-file-index`
keeps its `#![forbid(unsafe_code)]` untouched.

No new external dependency: `libc` is already a workspace dependency and carries every `FAN_*`
constant and every prototype needed.

## Running the harness

```sh
cargo run --release -p templar-fanotify --example fanotify-lab -- probe /tmp "$HOME"
cargo run --release -p templar-fanotify --example fanotify-lab -- arm --dirs 100000
cargo run --release -p templar-fanotify --example fanotify-lab -- events --files 20000
cargo run --release -p templar-fanotify --example fanotify-lab -- burst  --files 20000
cargo run --release -p templar-fanotify --example fanotify-lab -- stall  --files 40000
sudo scripts/fanotify-privileged-probe.sh    # the only step that needs root
```

An example rather than a bench target on purpose: the index's fixture guard panics on a watch root
outside `$TMPDIR` whenever the running executable has a `deps` path component, which every test and
bench binary has. Every workload builds its own fixture under `$TMPDIR` and removes it; nothing
here touches a real home.
