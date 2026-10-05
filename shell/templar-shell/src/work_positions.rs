//! Soft per-folder centroids, separate from the persistent field document and window identity.
use crate::work_layout::{Arrangement, ClusterKey};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkPositions(pub BTreeMap<PathBuf, (u16, u16)>);
impl WorkPositions {
    pub fn target(&self, folder: &Path, cols: u16, rows: u16) -> Option<(i64, i64, i64)> {
        let &(x, y) = self.0.get(folder)?;
        Some((
            i64::from(x) * i64::from(cols) * 2,
            i64::from(y) * i64::from(rows) * 2,
            65535,
        ))
    }
    /// Remember only folders, never ephemeral window ids, persistent tiles or unfiled tools.
    pub fn observe(&mut self, arrangement: &Arrangement) -> bool {
        let (cols, rows) = arrangement.grid;
        if cols == 0 || rows == 0 {
            return false;
        }
        let mut sums: BTreeMap<&Path, (u64, u64, u64)> = BTreeMap::new();
        for slot in &arrangement.slots {
            if let ClusterKey::Folder(folder) = &slot.cluster {
                let sum = sums.entry(folder).or_default();
                sum.0 +=
                    2 * u64::from(slot.placement.cell.col) + u64::from(slot.placement.span.cols);
                sum.1 +=
                    2 * u64::from(slot.placement.cell.row) + u64::from(slot.placement.span.rows);
                sum.2 += 1;
            }
        }
        let mut changed = false;
        for (folder, (x, y, n)) in sums {
            let hint = (
                (x * 65535 / (n * 2 * u64::from(cols))) as u16,
                (y * 65535 / (n * 2 * u64::from(rows))) as u16,
            );
            if self.0.get(folder) != Some(&hint) {
                self.0.insert(folder.to_path_buf(), hint);
                changed = true;
            }
        }
        changed
    }
    pub fn path() -> Option<PathBuf> {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state"))
            })?;
        Some(state.join("templar-shell/work-positions.json"))
    }
    pub fn load() -> Self {
        Self::path()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }
    pub fn save(&self) -> anyhow::Result<()> {
        let Some(path) = Self::path() else {
            return Ok(());
        };
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        serde_json::to_writer(file.as_file_mut(), self)?;
        file.as_file().sync_all()?;
        file.persist(path)?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hints_scale_across_outputs_and_roundtrip_without_window_ids() {
        let folder = PathBuf::from("/work/project");
        let hints = WorkPositions(BTreeMap::from([(folder.clone(), (32767, 16383))]));
        let restored: WorkPositions =
            serde_json::from_str(&serde_json::to_string(&hints).unwrap()).unwrap();
        assert_eq!(restored, hints);
        let a = restored.target(&folder, 16, 8).unwrap();
        let b = restored.target(&folder, 8, 4).unwrap();
        assert_eq!((a.0, a.1), (b.0 * 2, b.1 * 2));
    }
}
