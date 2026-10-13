//! Typed settings schema with validation. Values live as text (TOML) so they are diffable;
//! every change is recorded in history. GUI, CLI and migrations all go through this schema.

use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use templar_state::Store;
use thiserror::Error;

/// Value types a setting may take.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "type", content = "value")]
pub enum SettingValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// One of the schema's declared choices.
    Choice(String),
    StrList(Vec<String>),
}

/// Constraints on one key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SettingSpec {
    pub key: String,
    pub description: String,
    pub default: SettingValue,
    /// Allowed values for [`SettingValue::Choice`].
    #[serde(default)]
    pub choices: Vec<String>,
    #[serde(default)]
    pub min: Option<i64>,
    #[serde(default)]
    pub max: Option<i64>,
    /// Requires session restart / hostd (polkit) to apply.
    #[serde(default)]
    pub privileged: bool,
}

#[derive(Debug, Error, PartialEq)]
pub enum SettingError {
    #[error("unknown setting `{0}`")]
    UnknownKey(String),
    #[error("setting `{key}` expects {expected}")]
    WrongType { key: String, expected: &'static str },
    #[error("setting `{key}` must be one of {choices:?}")]
    InvalidChoice { key: String, choices: Vec<String> },
    #[error("setting `{key}` has invalid configuration: {reason}")]
    InvalidConfiguration { key: String, reason: String },
    #[error("setting `{key}` must be within [{min:?}, {max:?}]")]
    OutOfRange {
        key: String,
        min: Option<i64>,
        max: Option<i64>,
    },
}

/// The schema: keys are dotted (`shell.home-surface.wallpaper`, `resources.mode`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsSchema {
    pub specs: BTreeMap<String, SettingSpec>,
}

impl SettingsSchema {
    pub fn add(&mut self, spec: SettingSpec) {
        self.specs.insert(spec.key.clone(), spec);
    }

    /// Validate a proposed value against the schema.
    pub fn validate(&self, key: &str, value: &SettingValue) -> Result<(), SettingError> {
        let spec = self
            .specs
            .get(key)
            .ok_or_else(|| SettingError::UnknownKey(key.to_owned()))?;
        if key == WALLPAPER_CONFIG_KEY
            && let SettingValue::Str(json) = value
        {
            parse_wallpaper_config(json).map_err(|error| SettingError::InvalidConfiguration {
                key: key.to_owned(),
                reason: format!("{error:#}"),
            })?;
        }
        if key == templar_types::launcher::PREFERENCES_KEY
            && let SettingValue::Str(json) = value
        {
            parse_launcher_preferences(json).map_err(|reason| {
                SettingError::InvalidConfiguration {
                    key: key.to_owned(),
                    reason,
                }
            })?;
        }
        match (&spec.default, value) {
            (SettingValue::Bool(_), SettingValue::Bool(_))
            | (SettingValue::Float(_), SettingValue::Float(_))
            | (SettingValue::Str(_), SettingValue::Str(_))
            | (SettingValue::StrList(_), SettingValue::StrList(_)) => Ok(()),
            (SettingValue::Int(_), SettingValue::Int(v)) => {
                let below = spec.min.is_some_and(|min| *v < min);
                let above = spec.max.is_some_and(|max| *v > max);
                if below || above {
                    return Err(SettingError::OutOfRange {
                        key: key.to_owned(),
                        min: spec.min,
                        max: spec.max,
                    });
                }
                Ok(())
            }
            (SettingValue::Choice(_), SettingValue::Choice(v)) => {
                if spec.choices.iter().any(|choice| choice == v) {
                    Ok(())
                } else {
                    Err(SettingError::InvalidChoice {
                        key: key.to_owned(),
                        choices: spec.choices.clone(),
                    })
                }
            }
            (default, _) => Err(SettingError::WrongType {
                key: key.to_owned(),
                expected: type_name(default),
            }),
        }
    }
}

fn type_name(value: &SettingValue) -> &'static str {
    match value {
        SettingValue::Bool(_) => "a boolean",
        SettingValue::Int(_) => "an integer",
        SettingValue::Float(_) => "a number",
        SettingValue::Str(_) => "a string",
        SettingValue::Choice(_) => "a choice",
        SettingValue::StrList(_) => "a list of strings",
    }
}

/// The distribution's baseline schema (extended by components at startup).
pub fn baseline_schema() -> SettingsSchema {
    let mut schema = SettingsSchema::default();
    schema.add(SettingSpec {
        key: crate::file_view::FILE_VIEW_SETTING.into(),
        description: "File view preferences: the default sort/view and per-folder overrides, \
                      as one JSON value managed through crate::file_view"
            .into(),
        default: SettingValue::Str(String::new()),
        choices: vec![],
        min: None,
        max: None,
        privileged: false,
    });
    schema.add(SettingSpec {
        key: "shell.wallpaper.enabled".into(),
        description:
            "Show a wallpaper behind the Home Surface (off by default; never a functional layer)"
                .into(),
        default: SettingValue::Bool(false),
        choices: vec![],
        min: None,
        max: None,
        privileged: false,
    });
    schema.add(SettingSpec {
        key: WALLPAPER_CONFIG_KEY.into(),
        description: "Wallpaper source byte paths, rotation, geometry, texture/worker/transition budgets and per-output overrides as WallpaperConfig JSON".into(),
        default: SettingValue::Str(serde_json::to_string(&templar_types::wallpaper::WallpaperConfig::default()).expect("wallpaper defaults serialize")),
        choices: vec![],
        min: None,
        max: None,
        privileged: false,
    });
    schema.add(SettingSpec {
        key: "shell.dock.grouping".into(),
        description: "How the dock groups windows".into(),
        default: SettingValue::Choice("by-application".into()),
        choices: vec![
            "by-application".into(),
            "by-instance".into(),
            "separate-windows".into(),
        ],
        min: None,
        max: None,
        privileged: false,
    });
    add_appearance_specs(&mut schema);
    add_home_field_specs(&mut schema);
    add_home_garden_specs(&mut schema);
    add_rails_specs(&mut schema);
    add_notification_specs(&mut schema);
    add_window_tab_specs(&mut schema);
    schema.add(SettingSpec {
        key: templar_types::launcher::PREFERENCES_KEY.into(),
        description: "Saved launcher targets, display aliases and hidden search identities".into(),
        default: SettingValue::Str("{}".into()),
        choices: vec![],
        min: None,
        max: None,
        privileged: false,
    });
    schema.add(SettingSpec {
        key: "shell.launcher.recent-apps".into(),
        description: "Recently used applications, newest first, shown in the global launcher"
            .into(),
        default: SettingValue::StrList(Vec::new()),
        choices: vec![],
        min: None,
        max: None,
        privileged: false,
    });
    schema.add(SettingSpec {
        key: "resources.mode".into(),
        description: "Resource governor mode".into(),
        default: SettingValue::Choice("observe-only".into()),
        choices: vec!["observe-only".into(), "enforce".into()],
        min: None,
        max: None,
        privileged: false,
    });
    schema.add(SettingSpec {
        key: "resources.reserve-gib".into(),
        description: "Memory reserve below which pressure containment starts".into(),
        default: SettingValue::Int(3),
        choices: vec![],
        min: Some(1),
        max: Some(64),
        privileged: false,
    });
    schema.add(SettingSpec {
        key: "hardware.gpu-mode".into(),
        description: "Hybrid graphics mode (one declarative setting, applied by hostd)".into(),
        default: SettingValue::Choice("hybrid".into()),
        choices: vec!["hybrid".into(), "integrated".into(), "discrete".into()],
        min: None,
        max: None,
        privileged: true,
    });
    schema.add(SettingSpec {
        key: "storage.reserve-percent".into(),
        description: "Percentage of the root volume reserved for login, update and recovery".into(),
        default: SettingValue::Int(5),
        choices: vec![],
        min: Some(1),
        max: Some(25),
        privileged: true,
    });
    schema
}

/// Shared appearance keys, observed live by owned surfaces.
fn add_appearance_specs(schema: &mut SettingsSchema) {
    schema.add(SettingSpec {
        key: "shell.appearance.window-transition".into(),
        description: "Window minimize and restore effect".into(),
        default: SettingValue::Choice("essence".into()),
        choices: vec!["off".into(), "simple".into(), "essence".into()],
        min: None,
        max: None,
        privileged: false,
    });
    schema.add(SettingSpec {
        key: "shell.appearance.particle-size".into(),
        description: "Essence particle diameter in logical pixels".into(),
        default: SettingValue::Int(6),
        choices: vec![],
        min: Some(1),
        max: Some(24),
        privileged: false,
    });
    schema.add(SettingSpec {
        key: "shell.appearance.theme".into(),
        description: "Light or dark colours. Owned surfaces update live without changing focus, \
                      text or Home card positions"
            .into(),
        default: SettingValue::Choice("light".into()),
        choices: vec!["light".into(), "dark".into()],
        min: None,
        max: None,
        privileged: false,
    });
    schema.add(SettingSpec {
        key: "shell.appearance.accent".into(),
        description: "The accent colour the shell highlights with. `default` keeps the theme's \
                      own; every other value names one of the bundled accents"
            .into(),
        default: SettingValue::Choice("default".into()),
        choices: vec![
            "default".into(),
            "teal".into(),
            "blue".into(),
            "violet".into(),
            "green".into(),
            "amber".into(),
            "red".into(),
        ],
        min: None,
        max: None,
        privileged: false,
    });
    schema.add(SettingSpec {
        key: "shell.appearance.third-party".into(),
        description:
            "Align supported GTK and Qt gtk3 fonts, icons, light/dark theme and GTK motion \
                      for future launches. Turning this off restores Templar-owned keys without \
                      overwriting external edits; existing application reload is not guaranteed"
                .into(),
        default: SettingValue::Bool(false),
        choices: vec![],
        min: None,
        max: None,
        privileged: false,
    });
}

/// The Home Field's keys (`shell.home-field.*`), read by `templar-shell`.
fn add_home_field_specs(schema: &mut SettingsSchema) {
    for (key, description, default, choices, min, max) in [
        (
            "shell.home-field.tile-size",
            "Logical pixels per Home Field grid cell",
            SettingValue::Int(104),
            vec![],
            Some(80),
            Some(240),
        ),
        (
            "shell.home-field.throw",
            "The throw experiment: `off` places a tile exactly where it is released; `full` \
             projects a fast release along its velocity \
             (@templaros.decision.home-field-stable-bounded-semantic-grammar)",
            SettingValue::Choice("off".into()),
            vec!["off".into(), "full".into()],
            None,
            None,
        ),
        (
            "shell.home-field.previews",
            "Window previews on parked-session tiles: none, a snapshot on park, or live at the \
             preview rate",
            SettingValue::Choice("live".into()),
            vec!["off".into(), "static".into(), "live".into()],
            None,
            None,
        ),
        (
            "shell.home-field.preview-rate",
            "Frames per second a live preview may update at",
            SettingValue::Int(1),
            vec![],
            Some(1),
            Some(4),
        ),
        (
            "shell.home-field.reduced-motion",
            "No park, restore, zoom, throw or rail reveal animation; state marks remain",
            SettingValue::Bool(false),
            vec![],
            None,
            None,
        ),
    ] {
        schema.add(SettingSpec {
            key: key.into(),
            description: description.into(),
            default,
            choices,
            min,
            max,
            privileged: false,
        });
    }
}

/// Homegarden's levers (`shell.home-garden.*`), read by `templar-sessiond`'s HomeGarden1 host
/// (`@templaros.decision.homegarden-activity-grown-places`). Every duration is in garden time.
fn add_home_garden_specs(schema: &mut SettingsSchema) {
    for (key, description, default, choices, min, max) in [
        (
            "shell.home-garden.enabled",
            "Grow places on Home from the user's own activity (opens, launches, project entry); \
             off means no observation and no tending, and existing places stay ordinary folders",
            SettingValue::Bool(true),
            vec![],
            None,
            None,
        ),
        (
            "shell.home-garden.half-life-hours",
            "Garden hours (unlocked session runtime) over which a place's relevance halves while \
             it is untouched",
            SettingValue::Int(12),
            vec![],
            Some(1),
            Some(1000),
        ),
        (
            "shell.home-garden.half-life-sessions",
            "Later sessions over which a place's relevance halves while it is untouched",
            SettingValue::Int(4),
            vec![],
            Some(1),
            Some(100),
        ),
        (
            "shell.home-garden.max-contexts",
            "How many places the ledger remembers; the least relevant unpinned one is dropped past it",
            SettingValue::Int(64),
            vec![],
            Some(8),
            Some(512),
        ),
        (
            "shell.home-garden.home-corner",
            "The corner of Home that first growths cluster at",
            SettingValue::Choice("bottom-left".into()),
            vec![
                "bottom-left".into(),
                "bottom-right".into(),
                "top-left".into(),
                "top-right".into(),
            ],
            None,
            None,
        ),
        (
            "shell.home-garden.theme",
            "How grown places are drawn: `mineral` (ink branches, a shelf of books, a pile of \
             sheets), `orchard` (grown trees and clusters), `cushion` (shaded treemaps), `stone` \
             (dry-stone cells) or `radial` (rings and strata)",
            SettingValue::Choice("mineral".into()),
            vec![
                "mineral".into(),
                "orchard".into(),
                "cushion".into(),
                "stone".into(),
                "radial".into(),
            ],
            None,
            None,
        ),
        (
            "shell.home-garden.ignore",
            "Directory names (or absolute prefixes) under which activity is machine churn and \
             never grows a place; hidden directories are always ignored",
            SettingValue::StrList(
                crate::homegarden::DEFAULT_IGNORE
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect(),
            ),
            vec![],
            None,
            None,
        ),
    ] {
        schema.add(SettingSpec {
            key: key.into(),
            description: description.into(),
            default,
            choices,
            min,
            max,
            privileged: false,
        });
    }
}

/// The gesture rails' keys (`shell.rails.*`): read by `templar-shell` for what the rails show and
/// by the compositor for where the gate is, so both agree on one box without a second protocol.
fn add_rails_specs(schema: &mut SettingsSchema) {
    for (key, description, default, choices, min, max) in [
        (
            "shell.rails.mode",
            "`gesture` keeps the screen clear until the bottom-centre gesture reveals the rails; \
             `bar` keeps the classic island bar and cluster always on screen",
            SettingValue::Choice("gesture".into()),
            vec!["gesture".into(), "bar".into()],
            None,
            None,
        ),
        (
            "shell.rails.swap",
            "Running apps on the right rail and system icons on the left, instead of the reverse",
            SettingValue::Bool(false),
            vec![],
            None,
            None,
        ),
        (
            "shell.rails.lanes",
            "How the running-apps strip groups windows by kind of program: `off` keeps first-seen \
             order; `contiguous` keeps terminals together, ahead of everything else; `rows` \
             puts terminals on the strip's own row and the rest on a second row above it. The \
             lane is a fixed property of the program, never a reading of what it is doing",
            SettingValue::Choice("contiguous".into()),
            vec!["off".into(), "contiguous".into(), "rows".into()],
            None,
            None,
        ),
        (
            "shell.rails.gate-width",
            "Width, in logical pixels, of the invisible box at the bottom centre that the reveal \
             gesture starts in",
            SettingValue::Int(240),
            vec![],
            Some(120),
            Some(800),
        ),
        (
            "shell.rails.gate-height",
            "Height, in logical pixels, of that box, measured up from the bottom edge",
            SettingValue::Int(96),
            vec![],
            Some(40),
            Some(300),
        ),
        (
            "shell.rails.learning",
            "Outline the gate on screen, so a person can learn where the gesture starts",
            SettingValue::Bool(false),
            vec![],
            None,
            None,
        ),
        (
            "shell.rails.exceptions",
            "Application ids under whose windows the gate never arms, for apps that use the \
             bottom edge themselves",
            SettingValue::StrList(Vec::new()),
            vec![],
            None,
            None,
        ),
    ] {
        schema.add(SettingSpec {
            key: key.into(),
            description: description.into(),
            default,
            choices,
            min,
            max,
            privileged: false,
        });
    }
}

/// The terminal-completion notification policy (`shell.notifications.*`), read by
/// `templar notify terminal-completed` (see `crate::notifications`). New terminals pick up
/// hook-side prefilter values at startup; the completion route itself always reads live.
fn add_notification_specs(schema: &mut SettingsSchema) {
    for (key, description, default, choices, min, max) in [
        (
            crate::notifications::LONG_JOB_SECONDS_KEY,
            "Notify when a foreground terminal command ran at least this many seconds. A \
             reviewable default, not a measured optimum",
            SettingValue::Int(10),
            vec![],
            Some(0),
            Some(3_600),
        ),
        (
            crate::notifications::NOTIFY_EVERY_JOB_KEY,
            "Notify for every finished foreground job, however short. Canceled jobs still \
             need their own switch",
            SettingValue::Bool(false),
            vec![],
            None,
            None,
        ),
        (
            crate::notifications::NOTIFY_CANCELED_KEY,
            "Notify for jobs canceled from the keyboard (Ctrl-C), not just finished ones",
            SettingValue::Bool(false),
            vec![],
            None,
            None,
        ),
        (
            crate::notifications::SKIP_WHEN_FOCUSED_KEY,
            "Stay silent when the terminal that ran the job still holds keyboard focus. \
             Jobs whose focus is unknown still notify",
            SettingValue::Bool(true),
            vec![],
            None,
            None,
        ),
    ] {
        schema.add(SettingSpec {
            key: key.into(),
            description: description.into(),
            default,
            choices,
            min,
            max,
            privileged: false,
        });
    }
}

/// The window tab policy (`windows.tabs.*`): one horizontal title tab plus three
/// controls per window, fixed at the bottom-right by default, with optional pointer
/// following. The compositor reads these through the shell's `TabSettings` and the
/// `templar-shell-v2` `set_tab_policy` request; every reader parses the committed bytes
/// through one function, so Set/Reset/undo/restart converge on the same policy.
fn add_window_tab_specs(schema: &mut SettingsSchema) {
    for (key, description, default, choices) in [
        (
            WINDOW_TABS_PLACEMENT_KEY,
            "Where a window's title and controls sit: a right angle at the same corner, or \
             the title at one end of its edge and the controls at the other",
            SettingValue::Choice("same-corner".into()),
            vec!["same-corner".to_owned(), "opposite-corners".to_owned()],
        ),
        (
            WINDOW_TABS_FOLLOW_POINTER_KEY,
            "Follow the nearest client corner as the pointer approaches from any side. \
             Off keeps every window's tabs at the fixed corner",
            SettingValue::Bool(false),
            vec![],
        ),
        (
            WINDOW_TABS_FIXED_CORNER_KEY,
            "The corner tabs keep while follow-pointer is off",
            SettingValue::Choice("bottom-right".into()),
            vec![
                "top-left".to_owned(),
                "top-right".to_owned(),
                "bottom-left".to_owned(),
                "bottom-right".to_owned(),
            ],
        ),
        (
            WINDOW_TABS_CONTROLS_KEY,
            "When the three controls draw: revealed on hover approach, or always",
            SettingValue::Choice("hover".into()),
            vec!["hover".to_owned(), "always".to_owned()],
        ),
        (
            WINDOW_TABS_TITLE_KEY,
            "When a non-fullscreen window's title tab draws: always while visible, or \
             only with the hovered controls",
            SettingValue::Choice("always".into()),
            vec!["always".to_owned(), "hover".to_owned()],
        ),
        (
            WINDOW_TABS_DEFAULT_CORNER_KEY,
            "The corner a new window's tabs start at",
            SettingValue::Choice("bottom-right".into()),
            vec![
                "top-left".to_owned(),
                "top-right".to_owned(),
                "bottom-left".to_owned(),
                "bottom-right".to_owned(),
            ],
        ),
    ] {
        schema.add(SettingSpec {
            key: key.into(),
            description: description.into(),
            default,
            choices,
            min: None,
            max: None,
            privileged: false,
        });
    }
}

/// Values that override the schema's defaults; absent keys fall back to `spec.default`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsValues(BTreeMap<String, SettingValue>);

impl SettingsValues {
    /// The effective value of `key`: the stored override, or the schema default.
    pub fn effective(&self, schema: &SettingsSchema, key: &str) -> Option<SettingValue> {
        self.0
            .get(key)
            .cloned()
            .or_else(|| schema.specs.get(key).map(|spec| spec.default.clone()))
    }

    /// Every known key at its effective value, in key order.
    pub fn list(&self, schema: &SettingsSchema) -> Vec<(String, SettingValue)> {
        schema
            .specs
            .keys()
            .map(|key| {
                (
                    key.clone(),
                    self.effective(schema, key)
                        .expect("key came from the schema itself"),
                )
            })
            .collect()
    }

    /// The stored override only (not the default) — `None` means "not yet set".
    pub fn stored(&self, key: &str) -> Option<&SettingValue> {
        self.0.get(key)
    }

    /// Publish a durable override only after its enclosing transaction commits.
    pub fn set(&mut self, key: &str, value: SettingValue) {
        self.0.insert(key.to_owned(), value);
    }

    /// Publish a durable reset only after its enclosing transaction commits.
    pub fn reset(&mut self, key: &str) -> Option<SettingValue> {
        self.0.remove(key)
    }
}

/// Load every stored override.
pub fn load_values(store: &Store) -> Result<SettingsValues> {
    let connection = store.lock();
    // `settings_values` is also the generic key/value table other domains piggyback on for
    // small internal bookkeeping that is not a typed setting (the mimeapps-import marker, the
    // power-profile overrides, the location policy blob — see their own modules), always under
    // a `_internal.`-prefixed key by convention. Those rows are not `SettingValue` JSON, so they
    // are excluded here rather than skipped-on-parse-failure, which would silently swallow a
    // genuinely corrupt setting instead.
    let mut statement = connection
        .prepare("SELECT key, value_json FROM settings_values WHERE key NOT LIKE '\\_internal.%' ESCAPE '\\'")
        .context("preparing settings_values query")?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .context("reading settings_values")?;
    let mut values = BTreeMap::new();
    for row in rows {
        let (key, json) = row?;
        let value: SettingValue = serde_json::from_str(&json)
            .with_context(|| format!("parsing stored value for setting {key:?}"))?;
        values.insert(key, value);
    }
    Ok(SettingsValues(values))
}

/// Validate and persist one change, without publishing it to the in-memory observation.
/// The caller publishes `value` with [`SettingsValues::set`] only after the enclosing commit.
///
/// The caller (the daemon's `Settings1.Set` handler) still owns appending the general history
/// event and the `change_log` row in the *same* transaction — this function only knows about
/// settings storage, not about history phrasing.
pub fn apply_within(
    tx: &Transaction<'_>,
    schema: &SettingsSchema,
    values: &SettingsValues,
    key: &str,
    value: &SettingValue,
    origin: &str,
) -> Result<(Option<String>, String, i64)> {
    schema
        .validate(key, value)
        .map_err(|error| anyhow::anyhow!(error))?;
    let old_json = values
        .stored(key)
        .map(serde_json::to_string)
        .transpose()
        .context("serializing previous setting value")?;
    let new_json = serde_json::to_string(&value).context("serializing new setting value")?;

    tx.execute(
        "INSERT INTO settings_values (key, value_json, updated_ms) VALUES (?1, ?2, ?3)
         ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json, updated_ms = excluded.updated_ms",
        params![key, new_json, templar_state::now_ms()],
    )
    .context("persisting setting value")?;
    tx.execute(
        "INSERT INTO settings_changes (at_ms, key, old_json, new_json, origin) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![templar_state::now_ms(), key, old_json, new_json, origin],
    )
    .context("recording settings_changes audit row")?;

    Ok((old_json, new_json, tx.last_insert_rowid()))
}

/// Persist removal of an override without changing the observation. The caller publishes the
/// reset with [`SettingsValues::reset`] only after the enclosing transaction commits.
pub fn reset_within(
    tx: &Transaction<'_>,
    values: &SettingsValues,
    key: &str,
    origin: &str,
) -> Result<Option<String>> {
    let Some(old) = values.stored(key) else {
        return Ok(None);
    };
    let old_json = serde_json::to_string(&old).context("serializing reset setting value")?;
    tx.execute("DELETE FROM settings_values WHERE key = ?1", params![key])
        .context("deleting settings_values row")?;
    // `new_json` is NOT NULL: the JSON literal `null` means "reset to the schema default",
    // distinct from an absent column.
    tx.execute(
        "INSERT INTO settings_changes (at_ms, key, old_json, new_json, origin) VALUES (?1, ?2, ?3, 'null', ?4)",
        params![templar_state::now_ms(), key, old_json, origin],
    )
    .context("recording settings_changes reset row")?;
    Ok(Some(old_json))
}

/// Reverse a change recorded in the shared `change_log` (`History1.Undo`): `old_json` is exactly
/// what [`apply_within`] returned as the previous value. `None` means the key had no stored
/// override before the change being undone, so undoing it resets to the schema default.
pub fn revert_within(
    tx: &Transaction<'_>,
    key: &str,
    old_json: Option<&str>,
) -> Result<Option<SettingValue>> {
    let previous: Option<String> = tx
        .query_row(
            "SELECT value_json FROM settings_values WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(json) = old_json {
        let value: SettingValue =
            serde_json::from_str(json).context("parsing a settings change_log's stored value")?;
        if key == WALLPAPER_CONFIG_KEY {
            let SettingValue::Str(config) = &value else {
                anyhow::bail!("wallpaper history value is not a configuration string");
            };
            parse_wallpaper_config(config)?;
        }
        tx.execute(
            "INSERT INTO settings_values (key, value_json, updated_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json, updated_ms = excluded.updated_ms",
            params![key, json, templar_state::now_ms()],
        )
        .context("restoring a settings_values row")?;
        tx.execute(
            "INSERT INTO settings_changes (at_ms, key, old_json, new_json, origin) VALUES (?1, ?2, ?3, ?4, 'undo')",
            params![templar_state::now_ms(), key, previous, json],
        )?;
        Ok(Some(value))
    } else {
        tx.execute("DELETE FROM settings_values WHERE key = ?1", params![key])
            .context("removing a settings_values row")?;
        tx.execute(
            "INSERT INTO settings_changes (at_ms, key, old_json, new_json, origin) VALUES (?1, ?2, ?3, 'null', 'undo')",
            params![templar_state::now_ms(), key, previous],
        )?;
        Ok(None)
    }
}

pub const WALLPAPER_ENABLED_KEY: &str = "shell.wallpaper.enabled";
pub const WALLPAPER_CONFIG_KEY: &str = "shell.wallpaper.config";

pub fn parse_launcher_preferences(
    json: &str,
) -> std::result::Result<templar_types::launcher::LauncherPreferences, String> {
    if json.len() > 2 * 1024 * 1024 {
        return Err("Launcher preferences exceed their size limit".into());
    }
    let preferences: templar_types::launcher::LauncherPreferences =
        serde_json::from_str(json).map_err(|error| error.to_string())?;
    preferences.validate()?;
    Ok(preferences)
}

/// Legacy stored keys remain readable, but do not advertise an unimplemented runtime choice.
pub fn setting_capability(key: &str, privileged: bool) -> templar_types::settings::Capability {
    if matches!(key, "shell.dock.grouping" | "resources.mode") {
        templar_types::settings::Capability::Unsupported { reason: "Deprecated: this key has no runtime consumer; its saved value is retained for compatibility".into() }
    } else {
        templar_types::settings::Capability::Available {
            requires_restart: privileged,
        }
    }
}

/// Committed keys of the corner-following window tab policy (planning doc §3).
pub const WINDOW_TABS_PLACEMENT_KEY: &str = "windows.tabs.placement";
pub const WINDOW_TABS_FOLLOW_POINTER_KEY: &str = "windows.tabs.follow-pointer";
pub const WINDOW_TABS_FIXED_CORNER_KEY: &str = "windows.tabs.fixed-corner";
pub const WINDOW_TABS_CONTROLS_KEY: &str = "windows.tabs.controls";
pub const WINDOW_TABS_TITLE_KEY: &str = "windows.tabs.title";
pub const WINDOW_TABS_DEFAULT_CORNER_KEY: &str = "windows.tabs.default-corner";

pub fn parse_wallpaper_config(json: &str) -> Result<templar_types::wallpaper::WallpaperConfig> {
    anyhow::ensure!(
        json.len() <= 65_536,
        "wallpaper configuration exceeds 64 KiB"
    );
    let config = serde_json::from_str(json).context("parsing WallpaperConfig")?;
    templar_wallpaper::config::validate(&config)?;
    Ok(config)
}

/// Read only committed effective settings; actions never create a second policy authority.
pub fn wallpaper_policy(
    schema: &SettingsSchema,
    values: &SettingsValues,
) -> Result<(bool, templar_types::wallpaper::WallpaperConfig)> {
    let Some(SettingValue::Bool(enabled)) = values.effective(schema, WALLPAPER_ENABLED_KEY) else {
        anyhow::bail!("wallpaper enable setting is not a boolean");
    };
    let Some(SettingValue::Str(json)) = values.effective(schema, WALLPAPER_CONFIG_KEY) else {
        anyhow::bail!("wallpaper configuration setting is not a string");
    };
    Ok((enabled, parse_wallpaper_config(&json)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_configuration_and_motion_schema_reject_invalid_values() {
        let schema = baseline_schema();
        let key = templar_types::launcher::PREFERENCES_KEY;
        assert_eq!(
            schema.validate(key, &SettingValue::Str("{}".into())),
            Ok(())
        );
        assert!(
            schema
                .validate(
                    key,
                    &SettingValue::Str(
                        r#"{"visibility":{"hidden_paths_b64":["cmVsYXRpdmU="]}}"#.into()
                    )
                )
                .is_err()
        );
        assert!(
            schema
                .validate("shell.appearance.particle-size", &SettingValue::Int(25))
                .is_err()
        );
        assert!(
            schema
                .validate(
                    "shell.appearance.window-transition",
                    &SettingValue::Choice("material".into())
                )
                .is_err()
        );
        for key in ["shell.dock.grouping", "resources.mode"] {
            assert!(matches!(
                setting_capability(key, false),
                templar_types::settings::Capability::Unsupported { .. }
            ));
        }
    }

    #[test]
    fn schema_validates_types_ranges_and_choices() {
        let schema = baseline_schema();
        assert_eq!(
            schema.validate("shell.wallpaper.enabled", &SettingValue::Bool(true)),
            Ok(())
        );
        assert!(matches!(
            schema.validate("shell.wallpaper.enabled", &SettingValue::Int(1)),
            Err(SettingError::WrongType { .. })
        ));
        assert!(matches!(
            schema.validate("resources.reserve-gib", &SettingValue::Int(0)),
            Err(SettingError::OutOfRange { .. })
        ));
        assert_eq!(
            schema.validate("resources.mode", &SettingValue::Choice("enforce".into())),
            Ok(())
        );
        assert!(matches!(
            schema.validate("resources.mode", &SettingValue::Choice("yolo".into())),
            Err(SettingError::InvalidChoice { .. })
        ));
        assert_eq!(
            schema.validate("nope", &SettingValue::Bool(true)),
            Err(SettingError::UnknownKey("nope".into()))
        );
        assert_eq!(
            schema.validate(
                "shell.launcher.recent-apps",
                &SettingValue::StrList(vec!["org.templaros.Files".into()])
            ),
            Ok(())
        );
    }

    #[test]
    fn rails_schema_rejects_unknown_choices_and_out_of_bounds_gates() {
        let schema = baseline_schema();
        assert_eq!(
            schema.validate("shell.rails.mode", &SettingValue::Choice("bar".into())),
            Ok(())
        );
        assert!(matches!(
            schema.validate("shell.rails.mode", &SettingValue::Choice("hidden".into())),
            Err(SettingError::InvalidChoice { .. })
        ));
        for (key, min, max) in [
            ("shell.rails.gate-width", 120, 800),
            ("shell.rails.gate-height", 40, 300),
        ] {
            assert_eq!(schema.validate(key, &SettingValue::Int(min)), Ok(()));
            assert_eq!(schema.validate(key, &SettingValue::Int(max)), Ok(()));
            for outside in [min - 1, max + 1] {
                assert!(
                    matches!(
                        schema.validate(key, &SettingValue::Int(outside)),
                        Err(SettingError::OutOfRange { .. })
                    ),
                    "{key} accepted {outside}"
                );
            }
        }
        assert!(matches!(
            schema.validate("shell.rails.exceptions", &SettingValue::Str("org.a".into())),
            Err(SettingError::WrongType { .. })
        ));
    }

    #[test]
    fn apply_persists_validates_and_reset_returns_the_default() {
        let store = Store::in_memory().unwrap();
        let schema = baseline_schema();
        let mut values = load_values(&store).unwrap();
        assert_eq!(
            values.effective(&schema, "resources.mode"),
            Some(SettingValue::Choice("observe-only".into()))
        );

        let (old, _new, _id) = store
            .transaction(|tx| {
                apply_within(
                    tx,
                    &schema,
                    &values,
                    "resources.mode",
                    &SettingValue::Choice("enforce".into()),
                    "test",
                )
            })
            .unwrap();
        values.set("resources.mode", SettingValue::Choice("enforce".into()));
        assert_eq!(old, None);
        assert_eq!(
            values.effective(&schema, "resources.mode"),
            Some(SettingValue::Choice("enforce".into()))
        );

        // Reloading from the store observes the same override.
        let reloaded = load_values(&store).unwrap();
        assert_eq!(
            reloaded.effective(&schema, "resources.mode"),
            Some(SettingValue::Choice("enforce".into()))
        );

        // An invalid value is rejected before anything is written.
        let result = store.transaction(|tx| {
            apply_within(
                tx,
                &schema,
                &values,
                "resources.mode",
                &SettingValue::Choice("bogus".into()),
                "test",
            )
        });
        assert!(result.is_err());
        assert_eq!(
            values.effective(&schema, "resources.mode"),
            Some(SettingValue::Choice("enforce".into())),
            "rejected change must not mutate the in-memory values"
        );

        store
            .transaction(|tx| reset_within(tx, &values, "resources.mode", "test"))
            .unwrap();
        values.reset("resources.mode");
        assert_eq!(
            values.effective(&schema, "resources.mode"),
            Some(SettingValue::Choice("observe-only".into())),
            "reset falls back to the schema default"
        );
    }

    #[test]
    fn revert_restores_the_previous_value_or_the_default_when_there_was_none() {
        let store = Store::in_memory().unwrap();
        let schema = baseline_schema();
        let mut values = load_values(&store).unwrap();

        let (old, _new, _id) = store
            .transaction(|tx| {
                apply_within(
                    tx,
                    &schema,
                    &values,
                    "resources.mode",
                    &SettingValue::Choice("enforce".into()),
                    "test",
                )
            })
            .unwrap();
        values.set("resources.mode", SettingValue::Choice("enforce".into()));
        assert_eq!(old, None);

        let restored = store
            .transaction(|tx| revert_within(tx, "resources.mode", old.as_deref()))
            .unwrap();
        match restored {
            Some(value) => values.set("resources.mode", value),
            None => {
                values.reset("resources.mode");
            }
        }
        assert_eq!(
            values.effective(&schema, "resources.mode"),
            Some(SettingValue::Choice("observe-only".into())),
            "reverting a change with no prior override falls back to the default"
        );
        assert!(
            load_values(&store)
                .unwrap()
                .stored("resources.mode")
                .is_none()
        );
    }
    #[test]
    fn notification_policy_keys_carry_the_proposed_defaults_and_bounds() {
        let schema = baseline_schema();
        for (key, default) in [
            (
                crate::notifications::LONG_JOB_SECONDS_KEY,
                SettingValue::Int(10),
            ),
            (
                crate::notifications::NOTIFY_EVERY_JOB_KEY,
                SettingValue::Bool(false),
            ),
            (
                crate::notifications::NOTIFY_CANCELED_KEY,
                SettingValue::Bool(false),
            ),
            (
                crate::notifications::SKIP_WHEN_FOCUSED_KEY,
                SettingValue::Bool(true),
            ),
        ] {
            let spec = schema
                .specs
                .get(key)
                .unwrap_or_else(|| panic!("{key} is in the schema"));
            assert_eq!(spec.default, default, "{key} default");
            assert_eq!(schema.validate(key, &default), Ok(()));
        }
        assert_eq!(
            schema.validate(
                crate::notifications::LONG_JOB_SECONDS_KEY,
                &SettingValue::Int(3_600)
            ),
            Ok(())
        );
        assert!(matches!(
            schema.validate(
                crate::notifications::LONG_JOB_SECONDS_KEY,
                &SettingValue::Int(3_601)
            ),
            Err(SettingError::OutOfRange { .. })
        ));
        assert!(matches!(
            schema.validate(
                crate::notifications::NOTIFY_EVERY_JOB_KEY,
                &SettingValue::Int(1)
            ),
            Err(SettingError::WrongType { .. })
        ));
    }

    /// Child 4 (`settings-converge`, schema half): the six tab keys exist with the
    /// fixed bottom-right defaults, accept their documented values, and reject the rest.
    #[test]
    fn window_tab_schema_carries_the_policy_with_fixed_bottom_right_defaults() {
        let schema = baseline_schema();
        for (key, default) in [
            (
                WINDOW_TABS_PLACEMENT_KEY,
                SettingValue::Choice("same-corner".into()),
            ),
            (WINDOW_TABS_FOLLOW_POINTER_KEY, SettingValue::Bool(false)),
            (
                WINDOW_TABS_FIXED_CORNER_KEY,
                SettingValue::Choice("bottom-right".into()),
            ),
            (
                WINDOW_TABS_CONTROLS_KEY,
                SettingValue::Choice("hover".into()),
            ),
            (WINDOW_TABS_TITLE_KEY, SettingValue::Choice("always".into())),
            (
                WINDOW_TABS_DEFAULT_CORNER_KEY,
                SettingValue::Choice("bottom-right".into()),
            ),
        ] {
            let spec = schema
                .specs
                .get(key)
                .unwrap_or_else(|| panic!("{key} missing"));
            assert_eq!(spec.default, default, "{key} default");
            assert_eq!(schema.validate(key, &default), Ok(()));
        }
        assert_eq!(
            schema.validate(
                WINDOW_TABS_PLACEMENT_KEY,
                &SettingValue::Choice("opposite-corners".into())
            ),
            Ok(())
        );
        for (key, bad) in [
            (WINDOW_TABS_PLACEMENT_KEY, "diagonal"),
            (WINDOW_TABS_FIXED_CORNER_KEY, "middle"),
            (WINDOW_TABS_CONTROLS_KEY, "sometimes"),
            (WINDOW_TABS_TITLE_KEY, "never"),
            (WINDOW_TABS_DEFAULT_CORNER_KEY, "top-middle"),
        ] {
            assert!(
                matches!(
                    schema.validate(key, &SettingValue::Choice(bad.into())),
                    Err(SettingError::InvalidChoice { .. })
                ),
                "{key} accepted {bad:?}"
            );
        }
        assert!(matches!(
            schema.validate(WINDOW_TABS_FOLLOW_POINTER_KEY, &SettingValue::Int(1)),
            Err(SettingError::WrongType { .. })
        ));
    }

    /// Set then Reset converges back to the schema defaults through `effective`, whatever
    /// the overrides were: every reader of the committed bytes starts from the same place.
    #[test]
    fn window_tab_set_reset_converges_to_defaults() {
        let schema = baseline_schema();
        let mut values = SettingsValues::default();
        for key in [
            WINDOW_TABS_PLACEMENT_KEY,
            WINDOW_TABS_FOLLOW_POINTER_KEY,
            WINDOW_TABS_FIXED_CORNER_KEY,
            WINDOW_TABS_CONTROLS_KEY,
            WINDOW_TABS_TITLE_KEY,
            WINDOW_TABS_DEFAULT_CORNER_KEY,
        ] {
            let default = schema.specs[key].default.clone();
            assert_eq!(values.effective(&schema, key), Some(default));
        }
        values.set(
            WINDOW_TABS_PLACEMENT_KEY,
            SettingValue::Choice("opposite-corners".into()),
        );
        values.set(WINDOW_TABS_FOLLOW_POINTER_KEY, SettingValue::Bool(true));
        assert_eq!(
            values.effective(&schema, WINDOW_TABS_PLACEMENT_KEY),
            Some(SettingValue::Choice("opposite-corners".into()))
        );
        assert_eq!(
            values.effective(&schema, WINDOW_TABS_FOLLOW_POINTER_KEY),
            Some(SettingValue::Bool(true))
        );
        values.reset(WINDOW_TABS_PLACEMENT_KEY);
        values.reset(WINDOW_TABS_FOLLOW_POINTER_KEY);
        assert_eq!(
            values.effective(&schema, WINDOW_TABS_PLACEMENT_KEY),
            Some(SettingValue::Choice("same-corner".into()))
        );
        assert_eq!(
            values.effective(&schema, WINDOW_TABS_FOLLOW_POINTER_KEY),
            Some(SettingValue::Bool(false))
        );
    }

    #[test]
    fn wallpaper_configuration_validates_json_bounds_and_lossless_paths() {
        let schema = baseline_schema();
        let mut config = templar_types::wallpaper::WallpaperConfig {
            sources: vec![vec![b'/', b'a', 255, b'.', b'p', b'n', b'g']],
            interval_seconds: Some(300),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(parse_wallpaper_config(&json).unwrap(), config);
        assert_eq!(
            schema.validate(WALLPAPER_CONFIG_KEY, &SettingValue::Str(json)),
            Ok(())
        );
        config.interval_seconds = Some(299);
        assert!(
            schema
                .validate(
                    WALLPAPER_CONFIG_KEY,
                    &SettingValue::Str(serde_json::to_string(&config).unwrap())
                )
                .is_err()
        );
        assert!(
            schema
                .validate(WALLPAPER_CONFIG_KEY, &SettingValue::Str("{broken".into()))
                .is_err()
        );
        assert!(
            schema
                .validate(WALLPAPER_CONFIG_KEY, &SettingValue::Bool(true))
                .is_err()
        );
    }
}
