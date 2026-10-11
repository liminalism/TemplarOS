//! Pure state and native identities for the shell's one application/file launcher.

use std::path::PathBuf;
use templar_types::AppId;
pub use templar_types::launcher::{FavoriteId, LauncherMode};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LauncherTarget {
    App(AppId),
    File(PathBuf),
    Directory(PathBuf),
    Favorite(FavoriteId),
}

impl LauncherTarget {
    /// Metadata identity; display labels and lossy paths never select an object.
    pub fn identity(&self) -> String {
        match self {
            Self::App(id) => format!("app:{id}"),
            Self::File(path) | Self::Directory(path) => {
                format!("path:{}", templar_client::file_index::encode_path(path))
            }
            Self::Favorite(id) => format!("favorite:{id}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LauncherRow {
    pub target: LauncherTarget,
    pub label: String,
    pub detail: String,
    pub icon_id: Option<String>,
    pub running: bool,
    pub section: Option<&'static str>,
    pub size_bytes: Option<u64>,
    pub mtime_ms: Option<i64>,
}

#[derive(Clone, Debug, Default)]
pub struct LauncherState {
    pub open: bool,
    pub mode: LauncherMode,
    pub query: String,
    pub highlighted: usize,
    pub generation: u64,
    pub scope: Option<PathBuf>,
    pub selected: Option<LauncherTarget>,
}

impl LauncherState {
    fn invalidate(&mut self) {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("launcher generation exhausted");
    }

    pub fn show(&mut self, mode: LauncherMode, scope: Option<PathBuf>, query: String) {
        self.open = true;
        self.mode = mode;
        self.scope = scope;
        self.query = query;
        self.highlighted = 0;
        self.selected = None;
        self.invalidate();
    }

    pub fn switch_mode(&mut self) {
        self.mode = match self.mode {
            LauncherMode::Apps => LauncherMode::Files,
            LauncherMode::Files => LauncherMode::Apps,
        };
        self.highlighted = 0;
        self.selected = None;
        self.invalidate();
    }

    pub fn set_query(&mut self, query: String) -> bool {
        if self.query == query {
            return false;
        }
        self.query = query;
        self.highlighted = 0;
        self.selected = None;
        self.invalidate();
        true
    }

    pub fn dismiss(&mut self) {
        self.open = false;
        self.invalidate();
    }

    pub fn accepts(&self, generation: u64) -> bool {
        self.open && self.mode == LauncherMode::Files && self.generation == generation
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_preserves_text_and_fences_previous_lifecycle() {
        let mut state = LauncherState::default();
        state.show(LauncherMode::Apps, None, "fire".into());
        let apps = state.generation;
        state.switch_mode();
        assert_eq!(state.query, "fire");
        assert!(state.accepts(state.generation));
        assert!(!state.accepts(apps));
        let files = state.generation;
        state.dismiss();
        state.show(LauncherMode::Files, None, "fire".into());
        assert!(!state.accepts(files));
    }

    #[test]
    fn native_identity_is_independent_of_display_text() {
        use std::os::unix::ffi::OsStringExt;
        let target = LauncherTarget::File(PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/nonutf8-\xff".to_vec(),
        )));
        assert_eq!(
            target.identity(),
            format!(
                "path:{}",
                templar_file_index::query::base64(b"/tmp/nonutf8-\xff")
            )
        );
    }
}
