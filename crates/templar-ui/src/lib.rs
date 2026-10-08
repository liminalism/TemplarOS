//! Templar-owned mapping into the generic toolkit; no windowing or session dependency.
use pixelkit_ui::Theme;
use templar_types::appearance::{Appearance, Palette, spacing, typography};

pub fn theme_for(palette: &Palette) -> Theme {
    let mut theme = Theme::default();
    theme.background = palette.ground;
    theme.panel = palette.shell_fill;
    theme.panel_alt = palette.system_shell;
    theme.panel_edge = palette.edge;
    theme.text = palette.text;
    theme.text_dim = palette.text_dim;
    theme.alarm = palette.danger;
    theme.alarm_soft = palette.danger_soft;
    theme.good = palette.activity;
    theme.good_soft = palette.activity_soft;
    theme.accent = palette.accent;
    theme.accent_soft = palette.accent_soft;
    theme.hover = palette.hover_fill;
    theme.selection = palette.accent_soft;
    theme.row_height = spacing::ROW;
    theme.body = theme.body.with_size(typography::BODY);
    theme.heading = theme.heading.with_size(typography::HEADING);
    theme.padding = spacing::PAD;
    theme.corner_radius = spacing::TILE_RADIUS;
    theme.border_width = spacing::BORDER;
    theme.focus_width = spacing::FOCUS;
    theme
}

/// Updating appearance does not replace a surface, its focus, text, or layout.
/// Consumers key cached drawings with `revision`, or clear them when `update` returns true.
#[derive(Clone, Copy, Debug, Default)]
pub struct AppearanceState {
    appearance: Appearance,
    revision: u64,
}
impl AppearanceState {
    pub fn new(appearance: Appearance) -> Self { Self { appearance, revision: 0 } }
    pub fn update(&mut self, appearance: Appearance) -> bool {
        if self.appearance == appearance { return false; }
        self.appearance = appearance;
        self.revision = self.revision.checked_add(1).expect("appearance revision exhausted");
        true
    }
    pub fn appearance(&self) -> Appearance { self.appearance }
    pub fn theme(&self) -> Theme { theme_for(&self.appearance.palette) }
    pub fn revision(&self) -> u64 { self.revision }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn live_changes_invalidate_drawings_only_when_effective_state_changes() {
        let mut state = AppearanceState::default();
        assert!(!state.update(Appearance::default()));
        assert_eq!(state.revision(), 0);
        let dark = Appearance::from_settings("dark", "blue", true);
        assert!(state.update(dark));
        assert_eq!(state.revision(), 1);
        let theme = state.theme();
        assert_eq!(theme.background, dark.palette.ground);
        assert_eq!(theme.accent, dark.palette.accent);
        assert!(!state.update(dark));
        assert!(state.update(Appearance::default()));
        assert_eq!(state.revision(), 2);
    }
}
