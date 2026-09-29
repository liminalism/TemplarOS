//! Typed settings schema with validation. Values live as text (TOML) so they are diffable;
//! every change is recorded in history. GUI, CLI and migrations all go through this schema.

use anyhow::{Context, Result};
use rusqlite::{Transaction, params};
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
    schema.add(SettingSpec {
        key: "shell.launcher.recent-apps".into(),
        description: "Recently launched applications, newest first, shown under Home's search"
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

/// The shell's appearance keys (`shell.appearance.*`), read by `templar-shell` at startup.
fn add_appearance_specs(schema: &mut SettingsSchema) {
    schema.add(SettingSpec {
        key: "shell.appearance.theme".into(),
        description: "Light or dark shell colours. Applies at the next shell start, which is \
                      about half a second (templar-shell restarts on its own; the compositor does \
                      not need to)"
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
            SettingValue::Choice("static".into()),
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
            "No zoom or throw animation on the Home Field; the ghost still shows",
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

    fn set(&mut self, key: &str, value: SettingValue) {
        self.0.insert(key.to_owned(), value);
    }

    fn reset(&mut self, key: &str) -> Option<SettingValue> {
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

/// Validate and apply one change: persists the new value, appends to the `settings_changes`
/// audit trail (old/new JSON, for `History1.Undo`) and returns that audit row's ID.
///
/// The caller (the daemon's `Settings1.Set` handler) still owns appending the general history
/// event and the `change_log` row in the *same* transaction — this function only knows about
/// settings storage, not about history phrasing.
pub fn apply_within(
    tx: &Transaction<'_>,
    schema: &SettingsSchema,
    values: &mut SettingsValues,
    key: &str,
    value: SettingValue,
    origin: &str,
) -> Result<(Option<String>, String, i64)> {
    schema
        .validate(key, &value)
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

    values.set(key, value);
    Ok((old_json, new_json, tx.last_insert_rowid()))
}

/// Remove a stored override, returning to the schema default. Like [`apply_within`], leaves
/// history/undo bookkeeping to the caller.
pub fn reset_within(
    tx: &Transaction<'_>,
    values: &mut SettingsValues,
    key: &str,
    origin: &str,
) -> Result<Option<String>> {
    let Some(old) = values.reset(key) else {
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
    values: &mut SettingsValues,
    key: &str,
    old_json: Option<&str>,
) -> Result<()> {
    if let Some(json) = old_json {
        let value: SettingValue =
            serde_json::from_str(json).context("parsing a settings change_log's stored value")?;
        tx.execute(
            "INSERT INTO settings_values (key, value_json, updated_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT (key) DO UPDATE SET value_json = excluded.value_json, updated_ms = excluded.updated_ms",
            params![key, json, templar_state::now_ms()],
        )
        .context("restoring a settings_values row")?;
        values.set(key, value);
    } else {
        tx.execute("DELETE FROM settings_values WHERE key = ?1", params![key])
            .context("removing a settings_values row")?;
        values.reset(key);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// The rails' keys are read by two processes (shell and compositor) that each fall back to
    /// the default on a value they cannot use, so the schema is what keeps a stored value usable.
    #[test]
    fn rails_keys_default_to_the_gesture_and_bound_the_gate() {
        let schema = baseline_schema();
        let values = SettingsValues::default();
        for (key, default) in [
            ("shell.rails.mode", SettingValue::Choice("gesture".into())),
            ("shell.rails.swap", SettingValue::Bool(false)),
            ("shell.rails.gate-width", SettingValue::Int(240)),
            ("shell.rails.gate-height", SettingValue::Int(96)),
            ("shell.rails.learning", SettingValue::Bool(false)),
            ("shell.rails.exceptions", SettingValue::StrList(Vec::new())),
        ] {
            assert_eq!(
                values.effective(&schema, key),
                Some(default.clone()),
                "{key}"
            );
            assert_eq!(schema.validate(key, &default), Ok(()), "{key} default");
        }
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

        let (old, new, _id) = store
            .transaction(|tx| {
                apply_within(
                    tx,
                    &schema,
                    &mut values,
                    "resources.mode",
                    SettingValue::Choice("enforce".into()),
                    "test",
                )
            })
            .unwrap();
        assert_eq!(old, None);
        assert_eq!(new, "{\"type\":\"choice\",\"value\":\"enforce\"}");
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
                &mut values,
                "resources.mode",
                SettingValue::Choice("bogus".into()),
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
            .transaction(|tx| reset_within(tx, &mut values, "resources.mode", "test"))
            .unwrap();
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
                    &mut values,
                    "resources.mode",
                    SettingValue::Choice("enforce".into()),
                    "test",
                )
            })
            .unwrap();
        assert_eq!(old, None);

        store
            .transaction(|tx| revert_within(tx, &mut values, "resources.mode", old.as_deref()))
            .unwrap();
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
}
