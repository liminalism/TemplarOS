//! Typed settings schema with validation. Values live as text (TOML) so they are diffable;
//! every change is recorded in history. GUI, CLI and migrations all go through this schema.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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
    }
}
