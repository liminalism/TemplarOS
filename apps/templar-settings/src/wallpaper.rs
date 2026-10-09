//! Wallpaper controls only forward policy to Settings1 and actions to Wallpaper1.
use anyhow::{Context, Result, bail};
use pixelkit_raster::Rect;
use pixelkit_text::Align;
use pixelkit_ui::{ScrollState, TextFieldState, Ui};
use templar_client::{proxies::SettingsProxy, wallpaper::WallpaperClient};
use templar_types::wallpaper::{
    OutputOverride, SizingMode, SourceOrder, TextureLimit, WallpaperConfig, WallpaperStatus,
};

#[derive(Debug)]
pub(crate) enum WallpaperCommand {
    Inspect,
    Next,
    Previous,
    Pause,
    Resume,
    Refresh,
    Enabled(bool),
    ResetEnabled,
    Save(WallpaperConfig),
    Reset,
}
#[derive(Default, Debug)]
pub(crate) struct WallpaperState {
    pub status: Option<WallpaperStatus>,
    pub error: Option<String>,
    pub committed: bool,
}

pub(crate) async fn run(command: WallpaperCommand) -> WallpaperState {
    let committed = matches!(command, WallpaperCommand::Save(_) | WallpaperCommand::Reset);
    let operation = async {
        let connection = zbus::Connection::session().await.context("session bus")?;
        let client = WallpaperClient::new(&connection).await?;
        let attempted = match &command {
            WallpaperCommand::Enabled(enabled) => {
                let settings = SettingsProxy::new(&connection).await?;
                settings
                    .set(
                        "shell.wallpaper.enabled",
                        &serde_json::json!({"type":"bool", "value":enabled}).to_string(),
                        "templar-settings-gui",
                    )
                    .await
                    .map_err(anyhow::Error::from)
            }
            WallpaperCommand::ResetEnabled => {
                let settings = SettingsProxy::new(&connection).await?;
                settings
                    .reset("shell.wallpaper.enabled", "templar-settings-gui")
                    .await
                    .map_err(anyhow::Error::from)
            }
            WallpaperCommand::Save(config) => {
                let settings = SettingsProxy::new(&connection).await?;
                settings
                    .set(
                        "shell.wallpaper.config",
                        &serde_json::json!({"type":"str", "value":serde_json::to_string(config)?})
                            .to_string(),
                        "templar-settings-gui",
                    )
                    .await
                    .map_err(anyhow::Error::from)
            }
            WallpaperCommand::Reset => {
                let settings = SettingsProxy::new(&connection).await?;
                settings
                    .reset("shell.wallpaper.config", "templar-settings-gui")
                    .await
                    .map_err(anyhow::Error::from)
            }
            WallpaperCommand::Inspect => Ok(()),
            WallpaperCommand::Next => client.next().await.map(|_| ()),
            WallpaperCommand::Previous => client.previous().await.map(|_| ()),
            WallpaperCommand::Pause => client.pause().await.map(|_| ()),
            WallpaperCommand::Resume => client.resume().await.map(|_| ()),
            WallpaperCommand::Refresh => client.refresh().await.map(|_| ()),
        };
        let status = client.inspect().await;
        let write_ok = attempted.is_ok();
        Ok::<_, anyhow::Error>(WallpaperState {
            error: attempted
                .err()
                .or_else(|| {
                    status
                        .as_ref()
                        .err()
                        .map(|error| anyhow::anyhow!("{error:#}"))
                })
                .map(|error| format!("{error:#}")),
            status: status.ok(),
            committed: committed && write_ok,
        })
    }
    .await;
    operation.unwrap_or_else(|error| WallpaperState {
        status: None,
        error: Some(format!("{error:#}")),
        committed: false,
    })
}

pub(crate) fn path_text(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    if let Ok(path) = std::str::from_utf8(bytes)
        && path != "inherit"
        && !path.starts_with("hex:")
        && !path.contains(['\n', '\r'])
    {
        return path.to_owned();
    }
    let mut text = String::from("hex:");
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}
fn parse_path(text: &str) -> Result<Vec<u8>> {
    if let Some(hex) = text.strip_prefix("hex:") {
        if hex.len() % 2 != 0 || !hex.is_ascii() {
            bail!("hex: requires even-length hexadecimal Unix bytes");
        }
        hex.as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                u8::from_str_radix(std::str::from_utf8(pair)?, 16).context("invalid hex pathname")
            })
            .collect()
    } else {
        Ok(text.as_bytes().to_vec())
    }
}
fn ground_text(color: [u8; 4]) -> String {
    format!(
        "#{:02x}{:02x}{:02x}{:02x}",
        color[0], color[1], color[2], color[3]
    )
}
fn parse_ground(text: &str) -> Result<[u8; 4]> {
    let hex = text.strip_prefix('#').unwrap_or(text);
    let bytes = parse_path(&format!("hex:{hex}"))?;
    match bytes.as_slice() {
        [r, g, b] => Ok([*r, *g, *b, 255]),
        [r, g, b, a] => Ok([*r, *g, *b, *a]),
        _ => bail!("Ground must be #RRGGBB or #RRGGBBAA; service requires opaque alpha"),
    }
}
fn sizing_label(sizing: SizingMode) -> &'static str {
    match sizing {
        SizingMode::Fill => "Fill",
        SizingMode::Fit => "Fit",
        SizingMode::Stretch => "Stretch",
        SizingMode::Center => "Center",
        SizingMode::Actual => "Actual",
        SizingMode::Tile => "Tile",
    }
}
fn next_sizing(sizing: SizingMode) -> SizingMode {
    match sizing {
        SizingMode::Fill => SizingMode::Fit,
        SizingMode::Fit => SizingMode::Stretch,
        SizingMode::Stretch => SizingMode::Center,
        SizingMode::Center => SizingMode::Actual,
        SizingMode::Actual => SizingMode::Tile,
        SizingMode::Tile => SizingMode::Fill,
    }
}
fn texture_text(texture: TextureLimit) -> String {
    match texture {
        TextureLimit::Native => "native".into(),
        TextureLimit::MaxEdge { pixels } => format!("edge:{pixels}"),
        TextureLimit::MaxPixels { pixels } => format!("pixels:{pixels}"),
    }
}
fn parse_texture(text: &str) -> Result<TextureLimit> {
    if text == "native" {
        return Ok(TextureLimit::Native);
    }
    if let Some(pixels) = text.strip_prefix("edge:") {
        return Ok(TextureLimit::MaxEdge {
            pixels: pixels.parse().context("edge must be a u32 pixel count")?,
        });
    }
    if let Some(pixels) = text.strip_prefix("pixels:") {
        return Ok(TextureLimit::MaxPixels {
            pixels: pixels.parse().context("pixels must be a u64 pixel count")?,
        });
    }
    bail!("Texture must be native, edge:N or pixels:N")
}
fn text_field(ui: &mut Ui<'_>, rect: Rect, label: &str, field: &mut TextFieldState) -> bool {
    ui.label(
        Rect::new(rect.x, rect.y, rect.w, ui.px(26)),
        label,
        ui.theme.text_dim,
        Align::Left,
    );
    let old = field.text().to_owned();
    ui.text_field(
        field,
        Rect::new(rect.x, rect.y + ui.px(28), rect.w, ui.px(34)),
        label,
        true,
        false,
    );
    old != field.text()
}
fn labeled_button(ui: &mut Ui<'_>, rect: Rect, label: &str, value: &str) -> bool {
    ui.label(
        Rect::new(rect.x, rect.y, rect.w / 2, ui.px(34)),
        label,
        ui.theme.text,
        Align::Left,
    );
    ui.button(
        Rect::new(rect.x + rect.w / 2, rect.y, rect.w / 2, ui.px(34)),
        value,
    )
}

fn diagnostic_lines(ui: &mut Ui<'_>, rect: Rect, text: &str) {
    let height = ui.px(22);
    for (index, line) in text.lines().enumerate() {
        let y = rect.y + index as i32 * height;
        if y + height > rect.bottom() {
            break;
        }
        ui.label(
            Rect::new(rect.x, y, rect.w, height),
            line,
            ui.theme.text_dim,
            Align::Left,
        );
    }
}

pub(crate) struct WallpaperView {
    pub state: WallpaperState,
    draft: Option<WallpaperConfig>,
    dirty: bool,
    scroll: ScrollState,
    source: TextFieldState,
    interval: TextFieldState,
    ground: TextFieldState,
    texture: TextFieldState,
    worker: TextFieldState,
    transition: TextFieldState,
    quantize: TextFieldState,
    output_name: TextFieldState,
    output_ground: TextFieldState,
    output_texture: TextFieldState,
    output_source: TextFieldState,
    selected_output: Option<String>,
    output_sizing: Option<SizingMode>,
    local_error: Option<String>,
    drawn_fields: Vec<usize>,
}
impl Default for WallpaperView {
    fn default() -> Self {
        let mut view = Self {
            state: WallpaperState::default(),
            draft: None,
            dirty: false,
            scroll: ScrollState::new(),
            source: TextFieldState::new(),
            interval: TextFieldState::new(),
            ground: TextFieldState::new(),
            texture: TextFieldState::new(),
            worker: TextFieldState::new(),
            transition: TextFieldState::new(),
            quantize: TextFieldState::new(),
            output_name: TextFieldState::new(),
            output_ground: TextFieldState::new(),
            output_texture: TextFieldState::new(),
            output_source: TextFieldState::new(),
            selected_output: None,
            output_sizing: None,
            local_error: None,
            drawn_fields: Vec::new(),
        };
        view.select_output("");
        view
    }
}
impl WallpaperView {
    pub fn replace(&mut self, state: WallpaperState) {
        if let Some(status) = &state.status
            && (!self.dirty || state.committed)
        {
            self.draft = Some(status.config.clone());
            self.interval.set_text(
                status
                    .config
                    .interval_seconds
                    .map_or_else(|| "off".into(), |seconds| seconds.to_string()),
            );
            self.ground.set_text(ground_text(status.config.ground));
            self.texture.set_text(texture_text(status.config.texture));
            self.worker
                .set_text(status.config.worker_budget_bytes.to_string());
            self.transition
                .set_text(status.config.transition_budget_bytes.to_string());
            self.quantize.set_text(
                status
                    .config
                    .quantize_colors
                    .map_or_else(|| "off".into(), |colors| colors.to_string()),
            );
            self.dirty = false;
        }
        self.state = state;
    }
    pub fn caret_area(&self) -> Option<Rect> {
        let fields = [
            &self.source,
            &self.interval,
            &self.ground,
            &self.texture,
            &self.worker,
            &self.transition,
            &self.quantize,
            &self.output_name,
            &self.output_ground,
            &self.output_texture,
            &self.output_source,
        ];
        self.drawn_fields
            .iter()
            .find_map(|index| fields[*index].caret_area())
    }
    fn save_draft(&self) -> Result<WallpaperConfig> {
        let mut config = self
            .draft
            .clone()
            .context("No observed Wallpaper1 policy")?;
        config.interval_seconds = if self.interval.text() == "off" {
            None
        } else {
            Some(
                self.interval
                    .text()
                    .parse()
                    .context("Interval must be off or integer seconds (300–3600)")?,
            )
        };
        config.ground = parse_ground(self.ground.text())?;
        config.texture = parse_texture(self.texture.text())?;
        config.worker_budget_bytes = self
            .worker
            .text()
            .parse()
            .context("Worker budget must be an integer byte count")?;
        config.transition_budget_bytes = self
            .transition
            .text()
            .parse()
            .context("Transition budget must be an integer byte count")?;
        config.quantize_colors = if self.quantize.text() == "off" {
            None
        } else {
            Some(
                self.quantize
                    .text()
                    .parse()
                    .context("WuDP must be off or integer colors (1–256)")?,
            )
        };
        Ok(config)
    }
    fn select_output(&mut self, name: &str) {
        let value = self
            .draft
            .as_ref()
            .and_then(|config| config.outputs.get(name))
            .cloned()
            .unwrap_or_default();
        self.selected_output = Some(name.to_owned());
        self.output_name.set_text(name);
        self.output_sizing = value.sizing;
        self.output_ground
            .set_text(value.ground.map_or_else(|| "inherit".into(), ground_text));
        self.output_texture
            .set_text(value.texture.map_or_else(|| "inherit".into(), texture_text));
        self.output_source.set_text(
            value
                .source
                .as_deref()
                .map_or_else(|| "inherit".into(), path_text),
        );
    }
    fn update_output(&mut self) -> Result<()> {
        let name = self.output_name.text();
        if name.is_empty() {
            bail!("Enter an output name (the compositor DisplayId shown in status)");
        }
        let value = OutputOverride {
            sizing: self.output_sizing,
            ground: if self.output_ground.text() == "inherit" {
                None
            } else {
                Some(parse_ground(self.output_ground.text())?)
            },
            texture: if self.output_texture.text() == "inherit" {
                None
            } else {
                Some(parse_texture(self.output_texture.text())?)
            },
            source: if self.output_source.text() == "inherit" {
                None
            } else {
                Some(parse_path(self.output_source.text())?)
            },
        };
        let config = self.draft.as_mut().context("No observed policy")?;
        if let Some(old) = &self.selected_output
            && old != name
        {
            config.outputs.remove(old);
        }
        config.outputs.insert(name.to_owned(), value);
        self.selected_output = Some(name.to_owned());
        self.dirty = true;
        Ok(())
    }
    #[allow(clippy::too_many_lines)]
    pub fn draw(&mut self, ui: &mut Ui<'_>, area: Rect) -> Option<WallpaperCommand> {
        self.drawn_fields.clear();
        let mut wanted = None;
        // Drawing is exclusive: move the observation, not its potentially large source policy.
        let status = self.state.status.take();
        let source_count = self.draft.as_ref().map_or(0, |config| config.sources.len());
        let mut remove_source = None;
        let outputs: Vec<String> = self
            .draft
            .as_ref()
            .map_or_else(Vec::new, |config| config.outputs.keys().cloned().collect());
        let mut rows = vec![Row::Heading, Row::Actions, Row::Enabled, Row::Status];
        if self.local_error.is_some()
            || self.state.error.is_some()
            || status.as_ref().is_some_and(|status| status.error.is_some())
        {
            rows.push(Row::Error);
        }
        if let Some(status) = &status {
            rows.push(Row::CurrentSource);
            if let Some(applied) = &status.applied {
                rows.extend((0..applied.outputs.len()).map(Row::Applied));
            }
        }
        if self.draft.is_some() {
            rows.push(Row::SourcesHint);
            rows.extend((0..source_count).map(Row::Source));
            rows.extend([
                Row::AddSource,
                Row::Order,
                Row::Recursive,
                Row::Interval,
                Row::Sizing,
                Row::Ground,
                Row::Texture,
                Row::Worker,
                Row::Transition,
                Row::Quantize,
                Row::OutputHeading,
            ]);
            rows.extend((0..outputs.len()).map(Row::Output));
            rows.extend([
                Row::OutputName,
                Row::OutputSizing,
                Row::OutputGround,
                Row::OutputTexture,
                Row::OutputSource,
                Row::OutputApply,
                Row::Save,
            ]);
        }
        let heights: Vec<i32> = rows
            .iter()
            .map(|row| {
                ui.px(match row {
                    Row::AddSource
                    | Row::Interval
                    | Row::Ground
                    | Row::Texture
                    | Row::Worker
                    | Row::Transition
                    | Row::Quantize
                    | Row::OutputName
                    | Row::OutputGround
                    | Row::OutputTexture
                    | Row::OutputSource => 76,
                    Row::Applied(_) => 120,
                    Row::Status | Row::Error => 90,
                    _ => 48,
                })
            })
            .collect();
        let mut scroll = self.scroll;
        ui.scroll_list(&mut scroll, area, &heights, |ui, index, rect| {
            match rows[index] {
                Row::Heading => ui.label(rect, "Wallpaper · Settings1 policy / Wallpaper1 runtime", ui.theme.text, Align::Left),
                Row::Actions => {
                    let width = (rect.w - ui.px(32)) / 5;
                    for (i, (label, command)) in [("Previous", WallpaperCommand::Previous), ("Next", WallpaperCommand::Next), ("Pause", WallpaperCommand::Pause), ("Resume", WallpaperCommand::Resume), ("Refresh", WallpaperCommand::Refresh)].into_iter().enumerate() {
                        if ui.button(Rect::new(rect.x + i as i32 * (width + ui.px(8)), rect.y, width, ui.px(34)), label) { wanted = Some(command); }
                    }
                }
                Row::Enabled => {
                    if let Some(status) = &status {
                        let control = Rect::new(rect.x, rect.y, rect.w - ui.px(100), rect.h);
                        if labeled_button(ui, control, "Enabled (saved immediately)", if status.enabled { "On" } else { "Off" }) { wanted = Some(WallpaperCommand::Enabled(!status.enabled)); }
                        if ui.button(Rect::new(rect.right() - ui.px(92), rect.y, ui.px(92), ui.px(34)), "Reset") { wanted = Some(WallpaperCommand::ResetEnabled); }
                    } else if ui.button(rect, "Inspect Wallpaper1") { wanted = Some(WallpaperCommand::Inspect); }
                }
                Row::Status => {
                    let text = status.as_ref().map_or_else(|| "Wallpaper1 observation unavailable; no applied state assumed".into(), |status| {
                        let applied = status.applied.as_ref().map_or_else(|| "no compositor observation".into(),
                            |applied| format!("active={} · indexed supported={}", applied.active(), applied.indexed_supported));
                        format!("Desired enabled={} · paused={} · pending={}\nPolicy revision {}:{}\nApplied: {applied}", status.enabled, status.paused, status.pending, status.revision.owner_epoch, status.revision.generation)
                    });
                    diagnostic_lines(ui, rect, &text);
                }
                Row::CurrentSource => {
                    let text = status.as_ref().and_then(|status| status.source.as_deref()).map_or_else(|| "Current source: none".into(), |source| format!("Current source: {}", path_text(source)));
                    ui.label(rect, &text, ui.theme.text_dim, Align::Left);
                }
                Row::Applied(index) => {
                    if let Some(output) = status.as_ref().and_then(|status| status.applied.as_ref()).and_then(|applied| applied.outputs.get(index)) {
                        let revision = |value: Option<templar_types::settings::Revision>| value.map_or_else(|| "none".into(), |value| format!("{}:{}", value.owner_epoch, value.generation));
                        diagnostic_lines(ui, rect, &format!("Output: {}\nAccepted: {}\nPresented: {}\nBacking {} B · texture {} B · replicas {}\nError: {}", output.output.0, revision(output.accepted), revision(output.presented), output.source_bytes, output.texture_bytes, output.context_replicas, output.error.as_deref().unwrap_or("none")));
                    }
                }
                Row::SourcesHint => ui.label(rect, "Files / directories · raw Unix bytes: hex:2fff80 (never lossy UTF8)", ui.theme.text_dim, Align::Left),
                Row::Source(index) => {
                    let source = &self.draft.as_ref().expect("draft").sources[index];
                    ui.label(Rect::new(rect.x, rect.y, rect.w - ui.px(100), ui.px(34)), &path_text(source), ui.theme.text, Align::Left);
                    if ui.button(Rect::new(rect.right() - ui.px(92), rect.y, ui.px(92), ui.px(34)), "Remove") { remove_source = Some(index); }
                }
                Row::AddSource => {
                    self.drawn_fields.push(0);
                    let field = Rect::new(rect.x, rect.y, rect.w - ui.px(100), rect.h);
                    text_field(ui, field, "Add file / directory (UTF8 path or hex:bytes)", &mut self.source);
                    if ui.button(Rect::new(rect.right() - ui.px(92), rect.y + ui.px(28), ui.px(92), ui.px(34)), "Add") {
                        match parse_path(self.source.text()) {
                            Ok(path) => { self.draft.as_mut().expect("draft").sources.push(path); self.source.set_text(""); self.dirty = true; self.local_error = None; }
                            Err(error) => self.local_error = Some(error.to_string()),
                        }
                    }
                }
                Row::Order => {
                    let config = self.draft.as_mut().expect("draft");
                    if labeled_button(ui, rect, "Source order (click to cycle)", match config.order { SourceOrder::Random => "Random", SourceOrder::Sequential => "Sequence", SourceOrder::Shuffle => "Shuffle" }) {
                        config.order = match config.order { SourceOrder::Random => SourceOrder::Sequential, SourceOrder::Sequential => SourceOrder::Shuffle, SourceOrder::Shuffle => SourceOrder::Random }; self.dirty = true;
                    }
                }
                Row::Recursive => {
                    let config = self.draft.as_mut().expect("draft");
                    if labeled_button(ui, rect, "Recurse into source directories", if config.recursive { "On" } else { "Off" }) { config.recursive = !config.recursive; self.dirty = true; }
                }
                Row::Sizing => {
                    let config = self.draft.as_mut().expect("draft");
                    if labeled_button(ui, rect, "Sizing (click to cycle all six modes)", sizing_label(config.sizing)) { config.sizing = next_sizing(config.sizing); self.dirty = true; }
                }
                Row::Interval => { self.drawn_fields.push(1); self.dirty |= text_field(ui, rect, "Automatic rotation: off or 300–3600 seconds (5 min–1 hr)", &mut self.interval); }
                Row::Ground => { self.drawn_fields.push(2); self.dirty |= text_field(ui, rect, "Ground / alpha flattening: #RRGGBB or #RRGGBBAA", &mut self.ground); }
                Row::Texture => { self.drawn_fields.push(3); self.dirty |= text_field(ui, rect, "Texture storage: native / edge:N / pixels:N (independent of sizing)", &mut self.texture); }
                Row::Worker => { self.drawn_fields.push(4); self.dirty |= text_field(ui, rect, "Worker budget in bytes", &mut self.worker); }
                Row::Transition => { self.drawn_fields.push(5); self.dirty |= text_field(ui, rect, "Old + candidate transition budget in bytes", &mut self.transition); }
                Row::Quantize => { self.drawn_fields.push(6); self.dirty |= text_field(ui, rect, "Experimental WuDP: off or 1–256 colors (opt-in; no memory win implied)", &mut self.quantize); }
                Row::OutputHeading => {
                    ui.label(Rect::new(rect.x, rect.y, rect.w - ui.px(100), ui.px(34)), "Per-output overrides · inherit uses shared policy", ui.theme.text, Align::Left);
                    if ui.button(Rect::new(rect.right() - ui.px(92), rect.y, ui.px(92), ui.px(34)), "New") { self.select_output(""); }
                }
                Row::Output(index) => {
                    let name = &outputs[index];
                    if ui.button(Rect::new(rect.x, rect.y, rect.w - ui.px(100), ui.px(34)), name) { self.select_output(name); }
                    if ui.button(Rect::new(rect.right() - ui.px(92), rect.y, ui.px(92), ui.px(34)), "Remove") { self.draft.as_mut().expect("draft").outputs.remove(name); self.dirty = true; }
                }
                Row::OutputName => { self.drawn_fields.push(7); text_field(ui, rect, "Output name (compositor DisplayId)", &mut self.output_name); }
                Row::OutputSizing => {
                    if labeled_button(ui, rect, "Output sizing (click to cycle)", self.output_sizing.map_or("Inherit", sizing_label)) {
                        self.output_sizing = match self.output_sizing { None => Some(SizingMode::Fill), Some(SizingMode::Tile) => None, Some(sizing) => Some(next_sizing(sizing)) };
                    }
                }
                Row::OutputGround => { self.drawn_fields.push(8); text_field(ui, rect, "Output ground: inherit or #RRGGBB[AA]", &mut self.output_ground); }
                Row::OutputTexture => { self.drawn_fields.push(9); text_field(ui, rect, "Output texture: inherit / native / edge:N / pixels:N", &mut self.output_texture); }
                Row::OutputSource => { self.drawn_fields.push(10); text_field(ui, rect, "Output source: inherit / UTF8 path / hex:bytes", &mut self.output_source); }
                Row::OutputApply => {
                    if ui.button(rect, "Apply output override to draft (then Save policy)") {
                        self.local_error = self.update_output().err().map(|error| format!("{error:#}"));
                    }
                }
                Row::Save => {
                    let half = (rect.w - ui.px(8)) / 2;
                    if ui.button(Rect::new(rect.x, rect.y, half, ui.px(34)), if self.dirty { "Save policy (unsaved changes)" } else { "Save policy" }) {
                        match self.save_draft() { Ok(config) => { self.local_error = None; wanted = Some(WallpaperCommand::Save(config)); }, Err(error) => self.local_error = Some(format!("{error:#}")) }
                    }
                    if ui.button(Rect::new(rect.x + half + ui.px(8), rect.y, half, ui.px(34)), "Reset policy via Settings1") { wanted = Some(WallpaperCommand::Reset); }
                }
                Row::Error => {
                    let text = [self.local_error.as_deref(), self.state.error.as_deref(), status.as_ref().and_then(|status| status.error.as_deref())].into_iter().flatten().collect::<Vec<_>>().join("\n");
                    diagnostic_lines(ui, rect, &text);
                }
            }
        });
        if let Some(index) = remove_source {
            self.draft.as_mut().expect("draft").sources.remove(index);
            self.dirty = true;
        }
        self.state.status = status;
        self.scroll = scroll;
        wanted
    }
}
#[derive(Clone, Copy)]
enum Row {
    Heading,
    Actions,
    Enabled,
    Status,
    CurrentSource,
    Applied(usize),
    SourcesHint,
    Source(usize),
    AddSource,
    Order,
    Recursive,
    Interval,
    Sizing,
    Ground,
    Texture,
    Worker,
    Transition,
    Quantize,
    OutputHeading,
    Output(usize),
    OutputName,
    OutputSizing,
    OutputGround,
    OutputTexture,
    OutputSource,
    OutputApply,
    Save,
    Error,
}

pub(crate) async fn print() -> Result<String> {
    let state = run(WallpaperCommand::Inspect).await;
    let status = state.status.context(
        state
            .error
            .unwrap_or_else(|| "Wallpaper1 unavailable".into()),
    )?;
    Ok(format!(
        "Wallpaper desired/applied status:\n{}",
        serde_json::to_string_pretty(&status)?
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_round_trip_without_utf8_loss_or_prefix_ambiguity() {
        for bytes in [
            vec![b'/', 255, 128],
            b"hex:abc".to_vec(),
            b"inherit".to_vec(),
            b"/a\nb".to_vec(),
            b"/ordinary path".to_vec(),
        ] {
            assert_eq!(parse_path(&path_text(&bytes)).unwrap(), bytes);
        }
    }
    #[test]
    fn fields_are_representation_not_runtime_policy() {
        assert_eq!(
            parse_texture("edge:1024").unwrap(),
            TextureLimit::MaxEdge { pixels: 1024 }
        );
        assert_eq!(
            parse_texture("pixels:123456").unwrap(),
            TextureLimit::MaxPixels { pixels: 123456 }
        );
        assert_eq!(parse_ground("#122334").unwrap(), [0x12, 0x23, 0x34, 255]);
        assert!(parse_texture("automatic").is_err());
    }
    #[test]
    fn unavailable_is_not_an_applied_default() {
        let mut view = WallpaperView::default();
        view.replace(WallpaperState {
            status: None,
            error: Some("owner absent".into()),
            committed: false,
        });
        assert!(view.draft.is_none());
        assert!(view.save_draft().is_err());
    }
    #[test]
    fn runtime_observations_preserve_unsaved_policy_and_raw_output_source() {
        let status = WallpaperStatus {
            revision: templar_types::settings::Revision::default(),
            enabled: true,
            config: WallpaperConfig::default(),
            paused: false,
            source: None,
            pending: false,
            applied: None,
            error: None,
        };
        let mut view = WallpaperView::default();
        view.replace(WallpaperState {
            status: Some(status.clone()),
            error: None,
            committed: false,
        });
        view.interval.set_text("600");
        view.dirty = true;
        view.replace(WallpaperState {
            status: Some(status.clone()),
            error: None,
            committed: false,
        });
        assert_eq!(view.save_draft().unwrap().interval_seconds, Some(600));
        view.output_name.set_text("test-output");
        view.output_source.set_text("hex:696e6865726974");
        view.update_output().unwrap();
        view.select_output("test-output");
        assert_eq!(view.output_source.text(), "hex:696e6865726974");
        assert_eq!(
            view.save_draft().unwrap().outputs["test-output"]
                .source
                .as_deref(),
            Some(b"inherit".as_slice())
        );
        view.replace(WallpaperState {
            status: Some(status),
            error: None,
            committed: true,
        });
        assert!(!view.dirty);
        assert_eq!(view.interval.text(), "off");
    }
}
