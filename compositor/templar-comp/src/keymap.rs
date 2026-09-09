//! Which keymap the seat gets, and where it comes from.
//!
//! `XkbConfig::default()` is every field empty, which makes xkbcommon fall back to the
//! `XKB_DEFAULT_*` environment variables and, failing those, to a bare US layout with no options.
//! That is fine for the person who wrote it and wrong for everyone else: a French or German
//! keyboard produces the wrong characters, and — because `options` is empty — there is no Compose
//! key at all, so an accent cannot be typed even on a layout that would otherwise offer one. The
//! failure is silent, and it looks exactly like a broken text field rather than a missing setting.
//!
//! On an Ubuntu core the answer is already written down. `/etc/default/keyboard` is what
//! `localectl`, `console-setup` and the installer all write, and it is the file every other
//! display manager on the system reads. So the order here is:
//!
//! 1. `XKB_DEFAULT_MODEL`/`LAYOUT`/`VARIANT`/`OPTIONS`/`RULES` — an explicit override, and the
//!    one thing a person can set for a single session without touching a system file.
//! 2. `/etc/default/keyboard` — the system's configured keyboard, honoured without anyone having
//!    to restate it in a TemplarOS-specific setting.
//! 3. Empty, which is xkbcommon's own default.
//!
//! Deliberately not a `Settings1` lookup. The seat is built while the compositor is coming up,
//! before there is a session bus to ask, and a keyboard that only works once a daemon answered is
//! a keyboard that cannot be used to fix the daemon.

use std::path::Path;

/// One resolved keymap, owned so it can outlive the file it was read from.
///
/// Smithay's `XkbConfig` borrows every field, so this is what the borrow points into; keep it
/// alive for as long as the config is in use (`XkbSource::config`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct XkbSource {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    /// `None` and `Some("")` mean different things to xkbcommon: `None` is "use the default
    /// options", `Some("")` is "no options at all". An unset or empty `XKBOPTIONS` is the former.
    pub options: Option<String>,
    /// Where this came from, for the startup log line — the whole point of which is that a person
    /// with the wrong layout can see what was read instead of guessing.
    pub origin: &'static str,
}

impl XkbSource {
    /// Resolve the keymap: environment first, then the system keyboard file, then nothing.
    pub fn resolve() -> XkbSource {
        Self::resolve_with(Path::new("/etc/default/keyboard"), |name| {
            std::env::var(name)
        })
    }

    /// The whole resolution, with the file path and the environment injected so it can be tested
    /// without a machine that has either.
    fn resolve_with<F>(keyboard_file: &Path, env: F) -> XkbSource
    where
        F: Fn(&str) -> Result<String, std::env::VarError>,
    {
        let from_env = |name: &str| env(name).ok().filter(|value| !value.is_empty());
        // Any one of these being set means the environment is speaking, and a partial override is
        // still an override — mixing half of it with a system file would produce a keymap nobody
        // configured.
        let names = [
            "XKB_DEFAULT_RULES",
            "XKB_DEFAULT_MODEL",
            "XKB_DEFAULT_LAYOUT",
            "XKB_DEFAULT_VARIANT",
            "XKB_DEFAULT_OPTIONS",
        ];
        if names.iter().any(|name| from_env(name).is_some()) {
            return XkbSource {
                rules: from_env("XKB_DEFAULT_RULES").unwrap_or_default(),
                model: from_env("XKB_DEFAULT_MODEL").unwrap_or_default(),
                layout: from_env("XKB_DEFAULT_LAYOUT").unwrap_or_default(),
                variant: from_env("XKB_DEFAULT_VARIANT").unwrap_or_default(),
                options: from_env("XKB_DEFAULT_OPTIONS"),
                origin: "XKB_DEFAULT_* environment",
            };
        }
        match std::fs::read_to_string(keyboard_file) {
            Ok(text) => Self::from_keyboard_file(&text),
            Err(error) => {
                tracing::debug!(%error, path = %keyboard_file.display(), "no system keyboard file");
                XkbSource {
                    origin: "xkbcommon default",
                    ..XkbSource::default()
                }
            }
        }
    }

    /// Parse `keyboard(5)`: `KEY="value"` or `KEY=value`, one per line, `#` comments. Anything
    /// unrecognised is ignored rather than refused — this file is shared with console-setup and
    /// carries keys that are none of our business (`BACKSPACE`, `XKBCOMPOSE` on some systems).
    fn from_keyboard_file(text: &str) -> XkbSource {
        let mut source = XkbSource {
            origin: "/etc/default/keyboard",
            ..XkbSource::default()
        };
        let mut saw_options = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches('"').trim_matches('\'').to_owned();
            match key.trim() {
                "XKBMODEL" => source.model = value,
                "XKBLAYOUT" => source.layout = value,
                "XKBVARIANT" => source.variant = value,
                "XKBOPTIONS" => {
                    // An empty `XKBOPTIONS=""` is the file saying "no options", which is not the
                    // same as the file not mentioning options at all.
                    saw_options = true;
                    source.options = Some(value);
                }
                _ => {}
            }
        }
        if !saw_options {
            source.options = None;
        }
        source
    }

    /// Borrow this as the config Smithay wants. The returned value borrows `self`.
    pub fn config(&self) -> smithay::input::keyboard::XkbConfig<'_> {
        smithay::input::keyboard::XkbConfig {
            rules: &self.rules,
            model: &self.model,
            layout: &self.layout,
            variant: &self.variant,
            options: self.options.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Result<String, std::env::VarError> {
        Err(std::env::VarError::NotPresent)
    }

    #[test]
    fn the_system_keyboard_file_is_parsed_quotes_and_all() {
        let source = XkbSource::from_keyboard_file(
            "# KEYBOARD CONFIGURATION FILE\n\
             \n\
             XKBMODEL=\"pc105\"\n\
             XKBLAYOUT=\"fr\"\n\
             XKBVARIANT=\"azerty\"\n\
             BACKSPACE=\"guess\"\n\
             XKBOPTIONS=\"compose:ralt\"\n",
        );
        assert_eq!(source.model, "pc105");
        assert_eq!(source.layout, "fr");
        assert_eq!(source.variant, "azerty");
        assert_eq!(source.options.as_deref(), Some("compose:ralt"));
    }

    /// `XKBOPTIONS=""` is the file saying "no options"; a file that never mentions options is
    /// saying nothing, and xkbcommon should apply its own. The two must not collapse.
    #[test]
    fn an_empty_options_line_differs_from_no_options_line() {
        assert_eq!(
            XkbSource::from_keyboard_file("XKBOPTIONS=\"\"\n").options,
            Some(String::new())
        );
        assert_eq!(
            XkbSource::from_keyboard_file("XKBLAYOUT=us\n").options,
            None
        );
    }

    #[test]
    fn the_environment_wins_whole_not_field_by_field() {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        std::fs::write(file.path(), "XKBLAYOUT=\"fr\"\nXKBMODEL=\"pc105\"\n").expect("write");
        // Only the layout is overridden; the model must NOT be taken from the file, because a
        // half-environment half-file keymap is one nobody configured.
        let env = |name: &str| match name {
            "XKB_DEFAULT_LAYOUT" => Ok("de".to_owned()),
            _ => Err(std::env::VarError::NotPresent),
        };
        let source = XkbSource::resolve_with(file.path(), env);
        assert_eq!(source.layout, "de");
        assert_eq!(source.model, "");
        assert_eq!(source.origin, "XKB_DEFAULT_* environment");
    }

    #[test]
    fn a_missing_keyboard_file_is_not_an_error() {
        let source = XkbSource::resolve_with(Path::new("/nonexistent/keyboard"), no_env);
        assert_eq!(
            source,
            XkbSource {
                origin: "xkbcommon default",
                ..XkbSource::default()
            }
        );
    }

    #[test]
    fn the_config_borrows_what_was_resolved() {
        let source = XkbSource::from_keyboard_file("XKBLAYOUT=\"gb\"\n");
        assert_eq!(source.config().layout, "gb");
    }
}
