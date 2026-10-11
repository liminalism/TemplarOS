//! Native editors for launcher metadata; all commits use atomic Settings1 mutations.
use pixelkit_raster::Rect;
use pixelkit_text::Align;
use pixelkit_ui::{Dropdown, ScrollState, TextFieldState, Ui};
use templar_types::launcher::{
    Favorite, FavoriteTarget, LauncherMutation, LauncherPreferences, app_identity, decode_path,
    encode_path,
};

pub(crate) struct LauncherView {
    pub preferences: LauncherPreferences,
    scroll: ScrollState,
    chooser: Dropdown,
    selected_app: usize,
    selected_app_id: Option<String>,
    editing: Option<String>,
    terminal: bool,
    label: TextFieldState,
    directory: TextFieldState,
    argument: TextFieldState,
    arguments: Vec<String>,
    alias_identity: Option<String>,
    alias: TextFieldState,
    focused: usize,
    serial: u64,
    pub error: Option<String>,
}
impl Default for LauncherView {
    fn default() -> Self {
        Self {
            preferences: LauncherPreferences::default(),
            scroll: ScrollState::new(),
            chooser: Dropdown::new(),
            selected_app: 0,
            selected_app_id: None,
            editing: None,
            terminal: false,
            label: TextFieldState::new(),
            directory: TextFieldState::new(),
            argument: TextFieldState::new(),
            arguments: vec![],
            alias_identity: None,
            alias: TextFieldState::new(),
            focused: 0,
            serial: 0,
            error: None,
        }
    }
}
#[derive(Clone)]
enum Row {
    Heading(&'static str),
    Favorite(usize),
    New,
    App,
    Kind,
    Label,
    Directory,
    Argument,
    Arg(usize),
    Save,
    Alias(String, String),
    AliasEdit,
    Hidden(String, String),
    Empty,
}
impl LauncherView {
    fn edit(&mut self, favorite: &Favorite, apps: &[(String, String)]) {
        self.editing = Some(favorite.id.clone());
        self.label.set_text(&favorite.label);
        self.selected_app_id = Some(favorite.target.app_id().to_owned());
        self.selected_app = apps
            .iter()
            .position(|app| app.0 == favorite.target.app_id())
            .unwrap_or(0);
        self.terminal = matches!(favorite.target, FavoriteTarget::Terminal { .. });
        if let FavoriteTarget::Terminal {
            cwd_b64, arguments, ..
        } = &favorite.target
        {
            self.directory.set_text(
                &decode_path(cwd_b64)
                    .map(|path| {
                        path.to_str().map_or_else(
                            || templar_types::uri_list::encode_path(&path),
                            str::to_owned,
                        )
                    })
                    .unwrap_or_default(),
            );
            self.arguments = arguments.clone();
        } else {
            self.arguments.clear();
            self.directory.set_text("");
        }
    }
    fn favorite(&mut self, apps: &[(String, String)]) -> Result<Favorite, String> {
        let (app_id, name) = apps
            .get(self.selected_app)
            .ok_or("No application catalogue is available")?;
        let target = if self.terminal {
            let text = self.directory.text();
            let path = if text.starts_with("file:") {
                templar_types::uri_list::decode_uri(text).ok_or("Use a local directory URI")?
            } else {
                std::path::PathBuf::from(text)
            };
            if !path.is_absolute() {
                return Err("Choose an absolute directory".into());
            }
            FavoriteTarget::Terminal {
                app_id: app_id.clone(),
                cwd_b64: encode_path(&path),
                arguments: self.arguments.clone(),
            }
        } else {
            FavoriteTarget::App {
                app_id: app_id.clone(),
            }
        };
        self.serial += 1;
        let id = self.editing.clone().unwrap_or_else(|| {
            format!(
                "favorite-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                self.serial
            )
        });
        let favorite = Favorite {
            id,
            label: if self.label.text().trim().is_empty() {
                name.clone()
            } else {
                self.label.text().to_owned()
            },
            target,
        };
        favorite.validate()?;
        Ok(favorite)
    }
    fn field(&mut self, ui: &mut Ui<'_>, rect: Rect, label: &str, id: usize) {
        ui.label(
            Rect::new(rect.x, rect.y, rect.w, ui.px(24)),
            label,
            ui.theme.text_dim,
            Align::Left,
        );
        let body = Rect::new(rect.x, rect.y + ui.px(26), rect.w, ui.px(34));
        if ui.input.take_click(body) || ui.input.focus_requested(body) {
            self.focused = id;
        }
        let field = match id {
            0 => &mut self.label,
            1 => &mut self.directory,
            2 => &mut self.argument,
            _ => &mut self.alias,
        };
        ui.text_field(field, body, label, self.focused == id, false);
    }
    pub(crate) fn draw(
        &mut self,
        ui: &mut Ui<'_>,
        area: Rect,
        names: &crate::names::AppNames,
    ) -> Option<LauncherMutation> {
        let mut apps = names.choices();
        if let Some(id) = &self.selected_app_id {
            if !apps.iter().any(|app| &app.0 == id) {
                apps.push((id.clone(), format!("{id} (unavailable)")));
            }
            self.selected_app = apps.iter().position(|app| &app.0 == id).unwrap_or(0);
        }
        self.selected_app = self.selected_app.min(apps.len().saturating_sub(1));
        let preferences = self.preferences.clone();
        let mut rows = vec![Row::Heading("Favorites")];
        rows.extend((0..preferences.favorites.len()).map(Row::Favorite));
        rows.extend([Row::New, Row::App, Row::Kind, Row::Label]);
        if self.terminal {
            rows.push(Row::Directory);
            rows.extend((0..self.arguments.len()).map(Row::Arg));
            rows.push(Row::Argument);
        }
        rows.push(Row::Save);
        rows.push(Row::Heading("Display names"));
        rows.extend(
            preferences
                .aliases
                .iter()
                .map(|(identity, label)| Row::Alias(identity.clone(), label.clone())),
        );
        if self.alias_identity.is_some() {
            rows.push(Row::AliasEdit);
        }
        rows.push(Row::Heading("Hidden items — targets are never deleted"));
        rows.extend(
            preferences
                .visibility
                .hidden_apps
                .iter()
                .map(|app| Row::Hidden(app_identity(app), names.name(app).to_owned())),
        );
        rows.extend(
            preferences
                .visibility
                .hidden_paths_b64
                .iter()
                .map(|encoded| {
                    Row::Hidden(
                        format!("path:{encoded}"),
                        decode_path(encoded)
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|_| "Unavailable path".into()),
                    )
                }),
        );
        if preferences.visibility.hidden_apps.is_empty()
            && preferences.visibility.hidden_paths_b64.is_empty()
        {
            rows.push(Row::Empty);
        }
        let heights: Vec<_> = rows
            .iter()
            .map(|row| {
                ui.px(
                    if matches!(
                        row,
                        Row::Label | Row::Directory | Row::Argument | Row::AliasEdit
                    ) {
                        76
                    } else {
                        42
                    },
                )
            })
            .collect();
        let mut scroll = self.scroll;
        let mut wanted = None;
        ui.scroll_list(&mut scroll, area, &heights, |ui, index, rect| {
            let scale = ui.scale;
            let button = |offset: i32, width: i32| {
                Rect::new(
                    rect.right() - scale.px(offset),
                    rect.y,
                    scale.px(width),
                    scale.px(34),
                )
            };
            match &rows[index] {
                Row::Heading(label) => ui.label(rect, label, ui.theme.text, Align::Left),
                Row::Favorite(index) => {
                    let favorite = &preferences.favorites[*index];
                    if ui.button(
                        Rect::new(rect.x, rect.y, (rect.w - ui.px(206)).max(1), ui.px(34)),
                        &favorite.label,
                    ) {
                        self.edit(favorite, &apps);
                    }
                    if *index > 0 && ui.button(button(200, 42), "↑") {
                        let mut ids: Vec<_> = preferences
                            .favorites
                            .iter()
                            .map(|entry| entry.id.clone())
                            .collect();
                        ids.swap(*index, *index - 1);
                        wanted = Some(LauncherMutation::ReorderFavorites { ids });
                    }
                    if *index + 1 < preferences.favorites.len() && ui.button(button(152, 42), "↓")
                    {
                        let mut ids: Vec<_> = preferences
                            .favorites
                            .iter()
                            .map(|entry| entry.id.clone())
                            .collect();
                        ids.swap(*index, *index + 1);
                        wanted = Some(LauncherMutation::ReorderFavorites { ids });
                    }
                    if ui.button(button(104, 104), "Remove") {
                        wanted = Some(LauncherMutation::RemoveFavorite {
                            id: favorite.id.clone(),
                        });
                    }
                }
                Row::New => {
                    if ui.button(rect, "New favorite") {
                        self.editing = None;
                        self.selected_app_id = None;
                        self.selected_app = 0;
                        self.label.set_text("");
                        self.arguments.clear();
                        self.directory.set_text("");
                        self.terminal = false;
                        self.error = None;
                    }
                }
                Row::App => {
                    let display_labels: Vec<_> = apps
                        .iter()
                        .map(|app| {
                            if names.name(&app.0) == app.0 {
                                app.1.clone()
                            } else {
                                names.qualified(&app.0)
                            }
                        })
                        .collect();
                    let labels: Vec<_> = display_labels.iter().map(String::as_str).collect();
                    if !labels.is_empty() {
                        ui.dropdown(&mut self.chooser, rect, &labels, self.selected_app);
                    } else {
                        ui.label(
                            rect,
                            "Applications unavailable",
                            ui.theme.text_dim,
                            Align::Left,
                        );
                    }
                }
                Row::Kind => {
                    if ui.button(
                        rect,
                        if self.terminal {
                            "Terminal profile (directory and arguments)"
                        } else {
                            "Application favorite — click for a terminal profile"
                        },
                    ) {
                        self.terminal = !self.terminal;
                    }
                }
                Row::Label => self.field(ui, rect, "Display name", 0),
                Row::Directory => self.field(ui, rect, "Directory", 1),
                Row::Arg(index) => {
                    if let Some(argument) = self.arguments.get(*index) {
                        ui.label(
                            Rect::new(rect.x, rect.y, rect.w - ui.px(106), rect.h),
                            argument,
                            ui.theme.text,
                            Align::Left,
                        );
                        if ui.button(button(104, 104), "Remove") {
                            self.arguments.remove(*index);
                        }
                    }
                }
                Row::Argument => {
                    self.field(
                        ui,
                        Rect::new(rect.x, rect.y, rect.w - ui.px(106), rect.h),
                        "One literal argument",
                        2,
                    );
                    if ui.button(button(104, 104), "Add") {
                        self.arguments.push(self.argument.text().to_owned());
                        self.argument.set_text("");
                    }
                }
                Row::Save => {
                    if ui.button(
                        rect,
                        if self.editing.is_some() {
                            "Save favorite"
                        } else {
                            "Add favorite"
                        },
                    ) {
                        match self.favorite(&apps) {
                            Ok(favorite) => {
                                wanted = Some(LauncherMutation::UpsertFavorite { favorite });
                                self.error = None;
                            }
                            Err(error) => self.error = Some(error),
                        }
                    }
                }
                Row::Alias(identity, label) => {
                    if ui.button(
                        Rect::new(rect.x, rect.y, rect.w - ui.px(106), ui.px(34)),
                        label,
                    ) {
                        self.alias_identity = Some(identity.clone());
                        self.alias.set_text(label);
                        self.focused = 3;
                    }
                    if ui.button(button(104, 104), "Reset name") {
                        wanted = Some(LauncherMutation::SetAlias {
                            identity: identity.clone(),
                            label: None,
                        });
                    }
                }
                Row::AliasEdit => {
                    self.field(
                        ui,
                        Rect::new(rect.x, rect.y, rect.w - ui.px(106), rect.h),
                        "Shortcut display name",
                        3,
                    );
                    if ui.button(button(104, 104), "Save name") {
                        wanted = self.alias_identity.clone().map(|identity| {
                            LauncherMutation::SetAlias {
                                identity,
                                label: Some(self.alias.text().to_owned()),
                            }
                        });
                    }
                }
                Row::Hidden(identity, label) => {
                    ui.label(
                        Rect::new(rect.x, rect.y, rect.w - ui.px(106), rect.h),
                        label,
                        ui.theme.text,
                        Align::Left,
                    );
                    if ui.button(button(104, 104), "Unhide") {
                        wanted = Some(LauncherMutation::SetHidden {
                            identity: identity.clone(),
                            hidden: false,
                        });
                    }
                }
                Row::Empty => ui.label(rect, "Nothing is hidden", ui.theme.text_dim, Align::Left),
            }
        });
        self.scroll = scroll;
        if let Some(index) = self.chooser.draw(ui, area) {
            self.selected_app = index;
            self.selected_app_id = apps.get(index).map(|app| app.0.clone());
        }
        wanted
    }
}

pub(crate) async fn mutate(command: LauncherMutation) -> Result<LauncherPreferences, String> {
    let connection = zbus::Connection::session()
        .await
        .map_err(|error| error.to_string())?;
    let proxy = templar_client::proxies::SettingsProxy::new(&connection)
        .await
        .map_err(|error| error.to_string())?;
    let json = proxy
        .update_launcher_v1(
            &serde_json::to_string(&command).map_err(|error| error.to_string())?,
            "templar-settings",
        )
        .await
        .map_err(|error| error.to_string())?;
    serde_json::from_str(&json).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};
    #[test]
    fn profile_editor_keeps_native_directory_and_literal_arguments() {
        let path = PathBuf::from(OsString::from_vec(b"/work/native-\xff".to_vec()));
        let original = Favorite {
            id: "stable".into(),
            label: "Development".into(),
            target: FavoriteTarget::Terminal {
                app_id: "org.example.Terminal".into(),
                cwd_b64: encode_path(&path),
                arguments: vec!["$(literal)".into(), "two words".into()],
            },
        };
        let apps = vec![("org.example.Terminal".into(), "Terminal".into())];
        let mut view = LauncherView::default();
        view.edit(&original, &apps);
        assert_eq!(view.favorite(&apps).unwrap(), original);
        view.directory.set_text("relative");
        assert!(view.favorite(&apps).is_err());
    }
}
