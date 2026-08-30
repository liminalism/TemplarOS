//! `org.freedesktop.impl.portal.FileChooser`.
//!
//! The real, fully-implemented part is agreeing with the rest of the system on "the last
//! folder": the starting directory comes from `Files.Recents1.SuggestDirectory` (unless the
//! caller already gave a more specific `current_folder`), and a result is written back through
//! `Files.Recents1.RecordDirectory` — so a sandboxed application and the file manager can never
//! disagree about where the user was last. The interactive multi-file picker itself needs a
//! toolkit that does not exist yet in this milestone; that part is the [`FileChooserUi`] seam.
//! [`NoUiAvailable`] is its honest default: no dialog exists, so nothing is fabricated and the
//! call is answered `CANCELLED`.

use crate::{
    clients::RecentsClient,
    request::{owned_value, response, with_request},
};
use async_trait::async_trait;
use lege_filed::{OperationType, Suggestion};
use std::{
    collections::HashMap,
    ffi::OsString,
    os::unix::ffi::OsStringExt as _,
    path::{Path, PathBuf},
    sync::Arc,
};
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

/// What kind of dialog was requested, and the options specific to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChooserKind {
    Open { multiple: bool, directory: bool },
    Save { current_name: Option<String> },
    SaveFiles { file_names: Vec<String> },
}

/// Everything a [`FileChooserUi`] needs to actually show something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChooserRequest {
    pub kind: ChooserKind,
    pub app_id: String,
    pub title: String,
    pub starting_directory: PathBuf,
}

/// What the user picked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChosenFiles {
    pub uris: Vec<String>,
    pub writable: bool,
}

/// The interactive part no toolkit exists to provide yet.
#[async_trait]
pub trait FileChooserUi: Send + Sync + std::fmt::Debug {
    async fn choose(&self, request: ChooserRequest) -> Option<ChosenFiles>;
}

/// Headless default: there is nothing to show, so nothing is chosen.
#[derive(Debug, Default)]
pub struct NoUiAvailable;

#[async_trait]
impl FileChooserUi for NoUiAvailable {
    async fn choose(&self, _request: ChooserRequest) -> Option<ChosenFiles> {
        None
    }
}

/// Pixelkit graphical chooser used by the live portal backend.
#[derive(Debug, Default)]
pub struct NativeUi;

#[async_trait]
impl FileChooserUi for NativeUi {
    async fn choose(&self, request: ChooserRequest) -> Option<ChosenFiles> {
        tokio::task::spawn_blocking(move || crate::dialog::files(request))
            .await
            .ok()
            .flatten()
    }
}

/// `OperationType` the shared last-directory policy should file this dialog under.
fn operation_type(kind: &ChooserKind) -> OperationType {
    match kind {
        ChooserKind::Open { .. } => OperationType::Open,
        ChooserKind::Save { .. } | ChooserKind::SaveFiles { .. } => OperationType::Save,
    }
}

/// The directory a suggestion resolves to, decoded from `Files.Recents1.SuggestDirectory`'s JSON.
fn suggested_directory(suggestion_json: &str) -> Option<PathBuf> {
    serde_json::from_str::<Suggestion>(suggestion_json)
        .ok()
        .map(|suggestion| suggestion.directory().clone())
}

/// Decide the starting directory: the caller's own `current_folder`, if given, beats our
/// suggestion (they know their own context better than we do); otherwise the shared policy's
/// answer, falling back to `home` if even that is unavailable. Takes `home` as a parameter
/// (rather than reading `$HOME` itself) so it stays a pure function: this crate's tests must run
/// with no process-global state, and `cargo test` runs a binary's tests concurrently, so a
/// helper that mutated `$HOME` for one test could race every other test reading it.
fn resolve_starting_directory(
    caller_current_folder: Option<&Path>,
    suggestion_json: &str,
    home: &Path,
) -> PathBuf {
    caller_current_folder
        .map(Path::to_path_buf)
        .or_else(|| suggested_directory(suggestion_json))
        .unwrap_or_else(|| home.to_path_buf())
}

/// A null-terminated `ay` byte-array path option, as sent by `current_folder`/`current_file`.
fn path_from_bytes(mut bytes: Vec<u8>) -> PathBuf {
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    PathBuf::from(OsString::from_vec(bytes))
}

fn get_bytes(options: &HashMap<String, OwnedValue>, key: &str) -> Option<Vec<u8>> {
    Vec::<u8>::try_from(options.get(key)?.clone()).ok()
}

fn get_bool(options: &HashMap<String, OwnedValue>, key: &str) -> bool {
    options
        .get(key)
        .and_then(|value| bool::try_from(value.clone()).ok())
        .unwrap_or(false)
}

fn get_string(options: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    options
        .get(key)
        .and_then(|value| String::try_from(value.clone()).ok())
}

/// The reply this backend sends: `(response, results)`.
fn reply(response_code: u32, chosen: Option<&ChosenFiles>) -> (u32, HashMap<String, OwnedValue>) {
    let mut results = HashMap::new();
    if let Some(chosen) = chosen {
        results.insert("uris".to_owned(), owned_value(chosen.uris.clone()));
        results.insert("writable".to_owned(), owned_value(chosen.writable));
    }
    (response_code, results)
}

/// `org.freedesktop.impl.portal.FileChooser`.
#[derive(Debug)]
pub struct FileChooserApi {
    pub recents: Arc<dyn RecentsClient>,
    pub ui: Arc<dyn FileChooserUi>,
}

impl FileChooserApi {
    /// Shared body for `OpenFile`/`SaveFile`/`SaveFiles`: resolve the starting directory, ask the
    /// UI seam, and — only on a real answer — record where the user ended up.
    async fn run(
        &self,
        kind: ChooserKind,
        app_id: &str,
        title: &str,
        options: &HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let operation = operation_type(&kind);
        let suggestion_json = self
            .recents
            .suggest_directory(app_id, operation.as_str())
            .await
            .unwrap_or_default();
        let caller_folder = get_bytes(options, "current_folder").map(path_from_bytes);
        let home = std::env::var("HOME").map_or_else(|_| PathBuf::from("/"), PathBuf::from);
        let starting_directory =
            resolve_starting_directory(caller_folder.as_deref(), &suggestion_json, &home);

        let request = ChooserRequest {
            kind,
            app_id: app_id.to_owned(),
            title: title.to_owned(),
            starting_directory: starting_directory.clone(),
        };

        match self.ui.choose(request).await {
            Some(chosen) => {
                if let Some(first) = chosen.uris.first()
                    && let Some(path) = lege_filed::actions::path_from_uri(first)
                    && let Some(directory) = path.parent()
                {
                    let _ = self
                        .recents
                        .record_directory(app_id, operation.as_str(), &directory.to_string_lossy())
                        .await;
                }
                reply(response::SUCCESS, Some(&chosen))
            }
            None => reply(response::CANCELLED, None),
        }
    }
}

#[zbus::interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooserApi {
    /// `parent_window` is part of the fixed spec signature on every method here; this backend
    /// needs no window-parenting (no toolkit dialog exists yet — see `FileChooserUi`).
    #[zbus(name = "OpenFile")]
    #[allow(unused_variables)]
    async fn open_file(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: OwnedObjectPath,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let kind = ChooserKind::Open {
            multiple: get_bool(&options, "multiple"),
            directory: get_bool(&options, "directory"),
        };
        with_request(connection, &handle, || {
            self.run(kind, app_id, title, &options)
        })
        .await
    }

    #[zbus(name = "SaveFile")]
    #[allow(unused_variables)]
    async fn save_file(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: OwnedObjectPath,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let kind = ChooserKind::Save {
            current_name: get_string(&options, "current_name"),
        };
        with_request(connection, &handle, || {
            self.run(kind, app_id, title, &options)
        })
        .await
    }

    #[zbus(name = "SaveFiles")]
    #[allow(unused_variables)]
    async fn save_files(
        &self,
        #[zbus(connection)] connection: &zbus::Connection,
        handle: OwnedObjectPath,
        app_id: &str,
        parent_window: &str,
        title: &str,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let kind = ChooserKind::SaveFiles {
            file_names: Vec::new(),
        };
        with_request(connection, &handle, || {
            self.run(kind, app_id, title, &options)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_callers_own_current_folder_beats_our_suggestion() {
        let suggestion = serde_json::to_string(&Suggestion::Fallback {
            directory: PathBuf::from("/home/u"),
        })
        .unwrap();
        let resolved = resolve_starting_directory(
            Some(Path::new("/home/u/specific")),
            &suggestion,
            Path::new("/home/u"),
        );
        assert_eq!(resolved, PathBuf::from("/home/u/specific"));
    }

    #[test]
    fn falls_back_to_the_shared_suggestion_when_the_caller_gives_none() {
        let suggestion = serde_json::to_string(&Suggestion::CurrentProject {
            project_id: "thesis".into(),
            directory: PathBuf::from("/home/u/thesis"),
        })
        .unwrap();
        assert_eq!(
            resolve_starting_directory(None, &suggestion, Path::new("/home/u")),
            PathBuf::from("/home/u/thesis")
        );
    }

    #[test]
    fn malformed_suggestion_json_falls_back_to_home() {
        assert_eq!(
            resolve_starting_directory(None, "not json", Path::new("/home/fallback")),
            PathBuf::from("/home/fallback")
        );
    }

    #[test]
    fn null_terminated_byte_paths_decode_correctly() {
        let mut bytes = b"/home/u/Downloads".to_vec();
        bytes.push(0);
        assert_eq!(path_from_bytes(bytes), PathBuf::from("/home/u/Downloads"));
        assert_eq!(
            path_from_bytes(b"/no/nul".to_vec()),
            PathBuf::from("/no/nul")
        );
    }

    #[derive(Debug, Default)]
    struct FakeRecents {
        recorded: std::sync::Mutex<Vec<(String, String, String)>>,
    }

    #[async_trait]
    impl RecentsClient for FakeRecents {
        async fn suggest_directory(
            &self,
            _app_id: &str,
            _operation: &str,
        ) -> anyhow::Result<String> {
            Ok(serde_json::to_string(&Suggestion::Fallback {
                directory: PathBuf::from("/home/u"),
            })
            .unwrap())
        }
        async fn record_directory(
            &self,
            app_id: &str,
            operation: &str,
            directory: &str,
        ) -> anyhow::Result<()> {
            self.recorded.lock().unwrap().push((
                app_id.to_owned(),
                operation.to_owned(),
                directory.to_owned(),
            ));
            Ok(())
        }
    }

    #[derive(Debug)]
    struct FixedChoice(Option<ChosenFiles>);

    #[async_trait]
    impl FileChooserUi for FixedChoice {
        async fn choose(&self, _request: ChooserRequest) -> Option<ChosenFiles> {
            self.0.clone()
        }
    }

    #[tokio::test]
    async fn no_ui_available_answers_cancelled() {
        let api = FileChooserApi {
            recents: Arc::new(FakeRecents::default()),
            ui: Arc::new(NoUiAvailable),
        };
        let (code, results) = api
            .run(
                ChooserKind::Open {
                    multiple: false,
                    directory: false,
                },
                "org.legeos.Test",
                "Open",
                &HashMap::new(),
            )
            .await;
        assert_eq!(code, response::CANCELLED);
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn a_chosen_file_is_reported_and_its_directory_recorded() {
        let recents = Arc::new(FakeRecents::default());
        let api = FileChooserApi {
            recents: recents.clone(),
            ui: Arc::new(FixedChoice(Some(ChosenFiles {
                uris: vec!["file:///home/u/report.pdf".to_owned()],
                writable: true,
            }))),
        };
        let (code, results) = api
            .run(
                ChooserKind::Open {
                    multiple: false,
                    directory: false,
                },
                "org.legeos.Test",
                "Open",
                &HashMap::new(),
            )
            .await;
        assert_eq!(code, response::SUCCESS);
        assert!(results.contains_key("uris"));
        let recorded = recents.recorded.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "org.legeos.Test");
        assert_eq!(recorded[0].2, "/home/u");
    }
}
