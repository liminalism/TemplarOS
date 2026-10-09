//! Reopens the durable files left by the owned WP17 debugger crash fixture.
//! The ordinary test suite does not create or inspect external fixture files.
use std::{path::PathBuf, sync::Arc};
use templar_file_index::{Index, IndexConfig, NativePath, RootId};
use templar_filed::delta_catchup::{Checkpoint, Checkpoints, DeltaCatchUp, DeltaStreamInfo};

#[derive(Debug)]
struct NoCheckpointCalls;
impl Checkpoints for NoCheckpointCalls {
    fn create(&self, _: &std::path::Path) -> anyhow::Result<Checkpoint> {
        panic!("pending crash fence must precede checkpoint creation")
    }
    fn list(&self) -> anyhow::Result<Vec<Checkpoint>> {
        panic!("pending crash fence must precede checkpoint lookup")
    }
    fn discard(&self, _: &str) -> anyhow::Result<()> {
        panic!("pending crash fence must preserve the old checkpoint")
    }
    fn delta_open(
        &self,
        _: &str,
        _: &str,
        _: &std::path::Path,
    ) -> anyhow::Result<(std::fs::File, DeltaStreamInfo)> {
        panic!("pending crash fence must prevent delta replay")
    }
}

#[test]
#[ignore = "scripts/test-index-crashes.sh supplies an owned killed-process fixture"]
fn killed_delta_reopens_with_unchanged_rows_and_a_replay_fence() {
    assert_eq!(
        std::env::var("TEMPLAR_INDEX_CRASH_PRIVATE_BUS").as_deref(),
        Ok("1")
    );
    let root =
        PathBuf::from(std::env::var_os("TEMPLAR_INDEX_CRASH_FIXTURE").expect("owned fixture"));
    assert!(root.join(".templar-index-crash-fixture").is_file());
    let cache = PathBuf::from(std::env::var_os("TEMPLAR_INDEX_CRASH_CACHE").expect("cache"));
    let state = PathBuf::from(std::env::var_os("TEMPLAR_INDEX_CRASH_STATE").expect("state"));
    assert!(cache.starts_with(&root) && state.starts_with(&root));
    let index = Index::open(&cache, IndexConfig::default()).unwrap();
    let status = index.root_status(RootId(1)).unwrap();
    assert!(status.path.to_path().starts_with(&root));
    assert!(
        index
            .entry(
                status.root,
                &NativePath::from_path(&status.path.to_path().join("notes.md"))
            )
            .unwrap()
            .is_none()
    );
    let backend = DeltaCatchUp::new(Arc::new(NoCheckpointCalls), state.clone());
    assert!(backend.try_catch_up(&index, &status, "").is_none());
    assert!(state.with_extension("root-1.pending").is_file());
    println!("killed delta: no published notes row; pending replay fence retained");
}
