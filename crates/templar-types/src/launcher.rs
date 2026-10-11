//! Persistent launcher metadata. Discovery preferences never modify their targets.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

pub const PREFERENCES_KEY: &str = "shell.launcher.preferences";
pub type FavoriteId = String;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LauncherMode {
    #[default]
    Apps,
    Files,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Favorite {
    pub id: FavoriteId,
    pub label: String,
    pub target: FavoriteTarget,
}
impl Favorite {
    /// A Home shortcut carries the same validated, self-contained launch target as the rail.
    pub fn validate(&self) -> Result<(), String> {
        text(&self.id, 128, "Favorite id")?;
        if self.id.is_empty() {
            return Err("Favorite id must be nonempty".into());
        }
        text(&self.label, 512, "Favorite label")?;
        self.target
            .app_id()
            .parse::<crate::AppId>()
            .map_err(|error| error.to_string())?;
        if let FavoriteTarget::Terminal {
            cwd_b64, arguments, ..
        } = &self.target
        {
            decode_path(cwd_b64)?;
            if arguments.len() > 64 {
                return Err("Too many terminal arguments".into());
            }
            for argument in arguments {
                text(argument, 4096, "Terminal argument")?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum FavoriteTarget {
    App {
        app_id: String,
    },
    Terminal {
        app_id: String,
        cwd_b64: String,
        #[serde(default)]
        arguments: Vec<String>,
    },
}
impl FavoriteTarget {
    pub fn app_id(&self) -> &str {
        match self {
            Self::App { app_id } | Self::Terminal { app_id, .. } => app_id,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LauncherVisibility {
    pub hidden_apps: BTreeSet<String>,
    pub hidden_paths_b64: BTreeSet<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LauncherPreferences {
    pub favorites: Vec<Favorite>,
    pub aliases: BTreeMap<String, String>,
    pub visibility: LauncherVisibility,
}

/// Granular changes are applied against the owner's current value, never a client's snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum LauncherMutation {
    UpsertFavorite {
        favorite: Favorite,
    },
    RemoveFavorite {
        id: FavoriteId,
    },
    ReorderFavorites {
        ids: Vec<FavoriteId>,
    },
    SetAlias {
        identity: String,
        label: Option<String>,
    },
    SetHidden {
        identity: String,
        hidden: bool,
    },
}

pub fn encode_path(path: &Path) -> String {
    STANDARD.encode(path.as_os_str().as_bytes())
}
pub fn decode_path(encoded: &str) -> Result<PathBuf, String> {
    if encoded.len() > 16_384 {
        return Err("Launcher path is too long".into());
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|error| format!("Invalid launcher path: {error}"))?;
    if STANDARD.encode(&bytes) != encoded || bytes.contains(&0) {
        return Err("Launcher path must be canonical base64 without NUL".into());
    }
    let path = PathBuf::from(OsString::from_vec(bytes));
    if !path.is_absolute() {
        return Err("Launcher path must be absolute".into());
    }
    Ok(path)
}
pub fn app_identity(app: &str) -> String {
    format!("app:{app}")
}
pub fn path_identity(path: &Path) -> String {
    format!("path:{}", encode_path(path))
}
pub fn validate_identity(identity: &str) -> Result<(), String> {
    if let Some(app) = identity.strip_prefix("app:") {
        app.parse::<crate::AppId>()
            .map(|_| ())
            .map_err(|error| error.to_string())
    } else if let Some(path) = identity.strip_prefix("path:") {
        decode_path(path).map(|_| ())
    } else {
        Err("Launcher identity must name an application or native path".into())
    }
}
fn text(value: &str, limit: usize, name: &str) -> Result<(), String> {
    if value.len() > limit || value.contains('\0') {
        Err(format!("{name} is too long or contains NUL"))
    } else {
        Ok(())
    }
}
impl LauncherPreferences {
    pub fn validate(&self) -> Result<(), String> {
        if self.favorites.len() > 128
            || self.aliases.len() > 4096
            || self.visibility.hidden_apps.len() > 4096
            || self.visibility.hidden_paths_b64.len() > 4096
        {
            return Err("Too many launcher preferences".into());
        }
        let mut ids = BTreeSet::new();
        let mut apps = BTreeSet::new();
        for favorite in &self.favorites {
            favorite.validate()?;
            if !ids.insert(&favorite.id) {
                return Err("Favorite ids must be unique".into());
            }
            match &favorite.target {
                FavoriteTarget::App { app_id } => {
                    if !apps.insert(app_id) {
                        return Err("An application can be favorited only once".into());
                    }
                }
                FavoriteTarget::Terminal { .. } => (),
            }
        }
        for (identity, label) in &self.aliases {
            validate_identity(identity)?;
            text(label, 512, "Launcher alias")?;
        }
        for app in &self.visibility.hidden_apps {
            app.parse::<crate::AppId>()
                .map_err(|error| error.to_string())?;
        }
        for path in &self.visibility.hidden_paths_b64 {
            decode_path(path)?;
        }
        Ok(())
    }
    /// A refused mutation leaves the original configuration intact.
    pub fn apply(&mut self, mutation: &LauncherMutation) -> Result<(), String> {
        let mut updated = self.clone();
        match mutation {
            LauncherMutation::UpsertFavorite { favorite } => {
                if let Some(existing) = updated
                    .favorites
                    .iter_mut()
                    .find(|entry| entry.id == favorite.id)
                {
                    *existing = favorite.clone();
                } else {
                    updated.favorites.push(favorite.clone());
                }
            }
            LauncherMutation::RemoveFavorite { id } => {
                updated.favorites.retain(|favorite| &favorite.id != id)
            }
            LauncherMutation::ReorderFavorites { ids } => {
                if ids.len() != updated.favorites.len()
                    || ids.iter().collect::<BTreeSet<_>>().len() != ids.len()
                    || ids
                        .iter()
                        .any(|id| !updated.favorites.iter().any(|favorite| &favorite.id == id))
                {
                    return Err("Reorder must name every favorite exactly once".into());
                }
                updated
                    .favorites
                    .sort_by_key(|favorite| ids.iter().position(|id| id == &favorite.id));
            }
            LauncherMutation::SetAlias { identity, label } => {
                validate_identity(identity)?;
                if let Some(label) = label.as_ref().filter(|label| !label.trim().is_empty()) {
                    updated.aliases.insert(identity.clone(), label.clone());
                } else {
                    updated.aliases.remove(identity);
                }
            }
            LauncherMutation::SetHidden { identity, hidden } => {
                validate_identity(identity)?;
                let (set, key) = if let Some(app) = identity.strip_prefix("app:") {
                    (&mut updated.visibility.hidden_apps, app)
                } else {
                    (
                        &mut updated.visibility.hidden_paths_b64,
                        identity.strip_prefix("path:").expect("validated path"),
                    )
                };
                if *hidden {
                    set.insert(key.to_owned());
                } else {
                    set.remove(key);
                }
            }
        }
        updated.validate()?;
        *self = updated;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_profile_and_independent_mutations_preserve_identity() {
        let path = PathBuf::from(OsString::from_vec(b"/work/name-\xff".to_vec()));
        let favorite = Favorite {
            id: "dev".into(),
            label: "Development".into(),
            target: FavoriteTarget::Terminal {
                app_id: "org.example.Terminal".into(),
                cwd_b64: encode_path(&path),
                arguments: vec!["literal $(no-command)".into()],
            },
        };
        let mut prefs = LauncherPreferences::default();
        prefs
            .apply(&LauncherMutation::UpsertFavorite { favorite })
            .unwrap();
        prefs
            .apply(&LauncherMutation::SetHidden {
                identity: path_identity(&path),
                hidden: true,
            })
            .unwrap();
        prefs
            .apply(&LauncherMutation::SetAlias {
                identity: app_identity("org.example.Editor"),
                label: Some("Notes".into()),
            })
            .unwrap();
        let roundtrip: LauncherPreferences =
            serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap();
        assert_eq!(roundtrip, prefs);
        assert_eq!(decode_path(&encode_path(&path)).unwrap(), path);
        assert_eq!(prefs.favorites.len(), 1);
        assert_eq!(prefs.visibility.hidden_paths_b64.len(), 1);
    }
    #[test]
    fn invalid_mutations_are_atomic_and_paths_never_lossy() {
        let mut prefs = LauncherPreferences::default();
        for encoded in ["cmVsYXRpdmU=", "L2EA", "not base64"] {
            assert!(decode_path(encoded).is_err());
        }
        let original = prefs.clone();
        assert!(
            prefs
                .apply(&LauncherMutation::SetHidden {
                    identity: "path:cmVsYXRpdmU=".into(),
                    hidden: true
                })
                .is_err()
        );
        assert_eq!(prefs, original);
        assert!(
            prefs
                .apply(&LauncherMutation::ReorderFavorites {
                    ids: vec!["missing".into()]
                })
                .is_err()
        );
    }
}
