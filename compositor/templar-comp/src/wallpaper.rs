//! Sealed candidates become static GLES textures; no resident raster mirror or repaint clock.
use smithay::{
    backend::{
        allocator::Fourcc,
        drm::DrmDeviceFd,
        renderer::{
            ContextId, ImportMem, Renderer, Texture,
            element::{
                Element, Id, Kind, RenderElement, RenderElementStates,
                solid::SolidColorRenderElement,
            },
            gles::{
                GlesError, GlesFrame, GlesPixelProgram, GlesRenderer, GlesTexture, Uniform,
                UniformName, UniformType, ffi,
            },
            multigpu::{MultiRenderer, gbm::GbmGlesBackend},
            utils::{CommitCounter, OpaqueRegions},
        },
    },
    output::Output,
    utils::{Buffer, Physical, Rectangle, Scale, user_data::UserDataMap},
};
use std::{collections::BTreeMap, sync::Arc};
use templar_types::{
    compositor_control::{ControlReply, ControlResponse},
    display::DisplayId,
    settings::Revision,
    wallpaper::{PixelFormat, WallpaperImage, WallpaperObservation, WallpaperOutput},
};
use templar_wallpaper::raster::RasterMapping;
use tokio::sync::watch;

type NativeApi = GbmGlesBackend<GlesRenderer, DrmDeviceFd>;
pub trait WallpaperRenderer: Renderer {
    fn wallpaper_gles(&mut self) -> &mut GlesRenderer;
    fn draw_essence(
        element: &crate::essence_renderer::EssenceElement,
        frame: &mut Self::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), Self::Error>;
    fn draw_wallpaper(
        element: &WallpaperElement,
        frame: &mut Self::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
    ) -> Result<(), Self::Error>;
}
impl WallpaperRenderer for GlesRenderer {
    fn wallpaper_gles(&mut self) -> &mut GlesRenderer {
        self
    }
    fn draw_essence(
        element: &crate::essence_renderer::EssenceElement,
        frame: &mut Self::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), Self::Error> {
        element.draw_gles(frame, src, dst, damage)
    }
    fn draw_wallpaper(
        element: &WallpaperElement,
        frame: &mut Self::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
    ) -> Result<(), Self::Error> {
        element.draw_gles(frame, src, dst, damage, opaque)
    }
}
impl WallpaperRenderer for MultiRenderer<'_, '_, NativeApi, NativeApi> {
    fn wallpaper_gles(&mut self) -> &mut GlesRenderer {
        self.as_mut()
    }
    fn draw_essence(
        element: &crate::essence_renderer::EssenceElement,
        frame: &mut Self::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
    ) -> Result<(), Self::Error> {
        element
            .draw_gles(frame.as_mut(), src, dst, damage)
            .map_err(smithay::backend::renderer::multigpu::Error::Render)
    }
    fn draw_wallpaper(
        element: &WallpaperElement,
        frame: &mut Self::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
    ) -> Result<(), Self::Error> {
        element
            .draw_gles(frame.as_mut(), src, dst, damage, opaque)
            .map_err(smithay::backend::renderer::multigpu::Error::Render)
    }
}

#[derive(Debug)]
struct Imported {
    image: WallpaperImage,
    texture: GlesTexture,
    palette: Option<GlesTexture>,
    program: GlesPixelProgram,
    context: ContextId<GlesTexture>,
    id: Id,
    ground_id: Id,
}
impl Imported {
    fn bytes(&self) -> u64 {
        self.image.raster.bytes + if self.palette.is_some() { 1024 } else { 0 }
    }
}
#[derive(Debug)]
struct ContextPrograms {
    id: ContextId<GlesTexture>,
    r8: bool,
    rgba: Option<GlesPixelProgram>,
    indexed: Option<GlesPixelProgram>,
}
impl ContextPrograms {
    fn program(
        &mut self,
        renderer: &mut GlesRenderer,
        indexed: bool,
    ) -> Result<GlesPixelProgram, String> {
        let cached = if indexed {
            &mut self.indexed
        } else {
            &mut self.rgba
        };
        if cached.is_none() {
            let uniforms = [
                UniformName::new("tex", UniformType::_1i),
                UniformName::new("wall_size", UniformType::_2f),
                UniformName::new("wall_tile", UniformType::_1f),
                UniformName::new("wall_palette", UniformType::_1i),
            ];
            *cached = Some(
                renderer
                    .compile_custom_pixel_shader(
                        if indexed { INDEX_SHADER } else { RGBA_SHADER },
                        &uniforms[..if indexed { 4 } else { 3 }],
                    )
                    .map_err(|error| error.to_string())?,
            );
        }
        Ok(cached
            .as_ref()
            .expect("compiled context-local program")
            .clone())
    }
}
struct Candidate {
    image: WallpaperImage,
    mapping: Option<RasterMapping>,
    imported: Option<Arc<Imported>>,
}
#[derive(Default)]
struct Entry {
    current: Option<Arc<Imported>>,
    candidate: Option<Candidate>,
    accepted: Option<Revision>,
    presented: Option<Revision>,
    error: Option<String>,
}
impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WallpaperEntry")
            .field("accepted", &self.accepted)
            .field("presented", &self.presented)
            .field("error", &self.error)
            .finish()
    }
}
#[derive(Debug)]
pub struct WallpaperState {
    revision: Revision,
    job_revision: Option<Revision>,
    entries: BTreeMap<String, Entry>,
    aliases: BTreeMap<String, String>,
    contexts: Vec<ContextPrograms>,
    subscribers: Vec<watch::Sender<Option<ControlReply>>>,
    indexed_supported: bool,
    last_published: Option<WallpaperObservation>,
    dirty: bool,
}
impl Default for WallpaperState {
    fn default() -> Self {
        Self {
            revision: Revision::new_owner(),
            job_revision: None,
            entries: BTreeMap::new(),
            aliases: BTreeMap::new(),
            contexts: Vec::new(),
            subscribers: Vec::new(),
            indexed_supported: false,
            last_published: None,
            dirty: true,
        }
    }
}
impl WallpaperState {
    pub fn bind_outputs(&mut self, outputs: Vec<(String, DisplayId)>) {
        self.aliases = outputs.into_iter().map(|(name, id)| (name, id.0)).collect();
    }
    fn canonical<'a>(&'a self, name: &'a str) -> &'a str {
        self.aliases.get(name).map_or(name, String::as_str)
    }
    pub fn observation(&self) -> WallpaperObservation {
        WallpaperObservation {
            revision: self.revision,
            indexed_supported: self.indexed_supported,
            outputs: self
                .entries
                .iter()
                .map(|(name, entry)| WallpaperOutput {
                    output: DisplayId(name.clone()),
                    accepted: entry.accepted,
                    presented: entry.presented,
                    source_bytes: entry
                        .candidate
                        .as_ref()
                        .filter(|c| c.mapping.is_some())
                        .map_or(0, |c| c.image.raster.bytes),
                    texture_bytes: entry.current.as_ref().map_or(0, |i| i.bytes())
                        + entry
                            .candidate
                            .as_ref()
                            .and_then(|c| c.imported.as_ref())
                            .map_or(0, |i| i.bytes()),
                    context_replicas: u32::from(
                        entry.current.is_some()
                            || entry
                                .candidate
                                .as_ref()
                                .is_some_and(|c| c.imported.is_some()),
                    ),
                    error: entry.error.clone(),
                })
                .collect(),
        }
    }
    pub fn subscribe(&mut self, sender: watch::Sender<Option<ControlReply>>) {
        self.subscribers.retain(|s| !s.is_closed());
        self.subscribers.push(sender);
    }
    pub fn publish(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let mut observation = self.observation();
        if self.last_published.as_ref() == Some(&observation) {
            return;
        }
        self.revision.generation = self.revision.generation.saturating_add(1);
        observation.revision = self.revision;
        self.last_published = Some(observation.clone());
        self.subscribers.retain(|s| !s.is_closed());
        for sender in &self.subscribers {
            sender.send_replace(Some(ControlReply {
                result: Ok(ControlResponse::Wallpaper(observation.clone())),
            }));
        }
    }
    fn changed(&mut self) {
        self.dirty = true;
        self.publish();
    }
    pub fn retain_outputs(&mut self, names: &[String]) {
        let before = self.entries.len();
        self.entries.retain(|name, _| names.contains(name));
        if self.entries.len() != before {
            self.changed();
        }
    }
    pub fn release_context(&mut self, context: &ContextId<GlesTexture>) {
        self.contexts.retain(|programs| &programs.id != context);
        for entry in self.entries.values_mut() {
            let lost_current = entry
                .current
                .as_ref()
                .is_some_and(|image| &image.context == context);
            let lost_candidate = entry
                .candidate
                .as_ref()
                .and_then(|candidate| candidate.imported.as_ref())
                .is_some_and(|image| &image.context == context);
            if lost_current {
                entry.current = None;
                entry.presented = None;
            }
            if lost_candidate {
                entry.candidate = None;
            }
            if lost_current || lost_candidate {
                entry.error = Some("Wallpaper GLES context was removed; re-decode the source for surviving outputs".into());
            }
        }
        self.indexed_supported =
            !self.contexts.is_empty() && self.contexts.iter().all(|programs| programs.r8);
        self.changed();
    }
    pub fn fence_topology(&mut self, revision: Revision) {
        for entry in self.entries.values_mut() {
            if entry
                .candidate
                .as_ref()
                .is_some_and(|candidate| candidate.image.display_revision != revision)
            {
                entry.candidate = None;
                entry.error = Some("Wallpaper candidate topology changed before presentation; re-decode for current outputs".into());
            }
        }
        self.changed();
    }
    pub fn reject(&mut self, output: &DisplayId, failure: String) {
        self.entries.entry(output.0.clone()).or_default().error = Some(failure);
        self.changed();
    }
    pub fn accept(&mut self, image: WallpaperImage, mapping: RasterMapping) -> Result<(), String> {
        // A clear/unplug removes resources, not the owner's monotonic job fence. Otherwise an
        // older FD already mapping on another control connection could resurrect disabled pixels.
        if self.job_revision.is_some_and(|revision| {
            revision.owner_epoch == image.revision.owner_epoch
                && revision.generation > image.revision.generation
        }) {
            return Err("A newer wallpaper generation superseded this descriptor".into());
        }
        if image.ground[3] != 255 {
            return Err("Wallpaper ground must be opaque".into());
        }
        if let Some(entry) = self.entries.get(&image.output.0)
            && let Some(accepted) = entry.accepted
            && accepted.owner_epoch == image.revision.owner_epoch
            && image.revision.generation < accepted.generation
        {
            return Err("A newer wallpaper generation was already accepted".into());
        }
        self.job_revision = Some(image.revision);
        // Latest candidate replaces the obsolete pending candidate, never queues a third raster.
        if let Some(entry) = self.entries.get_mut(&image.output.0) {
            entry.candidate = None;
        }
        let resident = self
            .entries
            .values()
            .try_fold(0u64, |sum, e| {
                let candidate = e.candidate.as_ref().map_or(0, |c| {
                    if let Some(imported) = &c.imported {
                        imported.bytes()
                    } else {
                        c.image.raster.bytes
                    }
                });
                sum.checked_add(e.current.as_ref().map_or(0, |i| i.bytes()))
                    .and_then(|sum| sum.checked_add(candidate))
            })
            .ok_or("Wallpaper resident accounting overflow")?;
        let texture = image
            .raster
            .bytes
            .checked_add(if image.raster.format == PixelFormat::Indexed8 {
                1024
            } else {
                0
            })
            .ok_or("Wallpaper texture accounting overflow")?;
        let overlap = resident
            .checked_add(image.raster.bytes)
            .and_then(|sum| sum.checked_add(texture))
            .ok_or("Wallpaper transition accounting overflow")?;
        if overlap > image.transition_budget_bytes {
            return Err(format!(
                "Wallpaper old + candidate backing + texture requires {overlap} bytes; transition limit is {}",
                image.transition_budget_bytes
            ));
        }
        let entry = self.entries.entry(image.output.0.clone()).or_default();
        entry.accepted = Some(image.revision);
        entry.error = None;
        entry.candidate = Some(Candidate {
            image,
            mapping: Some(mapping),
            imported: None,
        });
        self.changed();
        Ok(())
    }
    pub fn clear(&mut self, revision: Revision, output: Option<DisplayId>) -> Result<(), String> {
        if self.job_revision.is_some_and(|accepted| {
            accepted.owner_epoch == revision.owner_epoch
                && accepted.generation > revision.generation
        }) {
            return Err("A newer wallpaper generation superseded this clear".into());
        }
        let stale = |e: &Entry| {
            e.accepted.is_some_and(|r| {
                r.owner_epoch == revision.owner_epoch && r.generation > revision.generation
            })
        };
        if output.as_ref().map_or_else(
            || self.entries.values().any(stale),
            |id| self.entries.get(&id.0).is_some_and(stale),
        ) {
            return Err("A newer wallpaper generation superseded this clear".into());
        }
        self.job_revision = Some(revision);
        if let Some(output) = output {
            self.entries.remove(&output.0);
        } else {
            self.entries.clear();
        }
        self.changed();
        Ok(())
    }
    /// Called only with a current GLES context on the compositor thread. Mapping disappears after upload.
    pub fn elements<R: WallpaperRenderer>(
        &mut self,
        renderer: &mut R,
        output: &Output,
    ) -> Vec<WallpaperScene> {
        let gl = renderer.wallpaper_gles();
        let context = gl.context_id();
        let context_index = if let Some(index) = self
            .contexts
            .iter()
            .position(|programs| programs.id == context)
        {
            index
        } else {
            let r8 = supports_r8(gl);
            self.contexts.push(ContextPrograms {
                id: context.clone(),
                r8,
                rgba: None,
                indexed: None,
            });
            self.contexts.len() - 1
        };
        let all_supported = self.contexts.iter().all(|programs| programs.r8);
        if all_supported != self.indexed_supported {
            self.indexed_supported = all_supported;
            self.changed();
        }
        let name = output.name();
        let key = self
            .aliases
            .get(&name)
            .map_or(name.as_str(), String::as_str);
        let Some(entry) = self.entries.get_mut(key) else {
            return Vec::new();
        };
        let mut changed = false;
        if entry.current.as_ref().is_some_and(|i| i.context != context) {
            changed = true;
            entry.current = None;
            entry.presented = None;
            entry.error = Some(
                "Wallpaper GLES context was lost or replaced; re-decode the source for this output"
                    .into(),
            );
        }
        if let Some(candidate) = &mut entry.candidate {
            if candidate
                .imported
                .as_ref()
                .is_some_and(|i| i.context != context)
            {
                changed = true;
                candidate.imported = None;
                candidate.mapping = None;
                entry.error =
                    Some("Wallpaper candidate GLES context was lost; re-decode the source".into());
            }
            if let Some(mapping) = candidate.mapping.take() {
                changed = true;
                match import(
                    gl,
                    &candidate.image,
                    &mapping,
                    &mut self.contexts[context_index],
                ) {
                    Ok(imported) => candidate.imported = Some(Arc::new(imported)),
                    Err(failure) => {
                        entry.error = Some(format!(
                            "Wallpaper import failed; previous image retained: {failure}"
                        ))
                    }
                }
            }
        }
        if entry.error.is_some()
            && entry
                .candidate
                .as_ref()
                .is_some_and(|c| c.imported.is_none())
        {
            entry.candidate = None;
        }
        let imported = entry
            .candidate
            .as_ref()
            .and_then(|c| c.imported.clone())
            .or_else(|| entry.current.clone());
        let elements = imported.map_or_else(Vec::new, |imported| make_elements(imported, output));
        if changed {
            self.changed();
        }
        elements
    }
    pub fn frame_token(&self, output: &str, states: &RenderElementStates) -> Option<Revision> {
        let entry = self.entries.get(self.canonical(output))?;
        let image = entry
            .candidate
            .as_ref()
            .and_then(|c| c.imported.as_ref())
            .or(entry.current.as_ref())?;
        if entry.presented == Some(image.image.revision) && entry.candidate.is_none() {
            return None;
        }
        // Even an occluded image must be represented in a successfully submitted scene: Home
        // cannot become transparent until this attestation, so requiring visible pixels deadlocks.
        states
            .element_render_state(image.id.clone())
            .is_some()
            .then_some(image.image.revision)
    }
    pub fn frame_presented(&mut self, output: &str, revision: Option<Revision>) {
        let Some(revision) = revision else {
            return;
        };
        let key = self.aliases.get(output).map_or(output, String::as_str);
        let Some(entry) = self.entries.get_mut(key) else {
            return;
        };
        if entry
            .candidate
            .as_ref()
            .is_some_and(|c| c.image.revision == revision)
        {
            entry.current = entry.candidate.take().and_then(|c| c.imported);
            entry.error = None;
        }
        if entry
            .current
            .as_ref()
            .is_some_and(|i| i.image.revision == revision)
        {
            entry.presented = Some(revision);
        }
        self.changed();
    }
    pub fn frame_failed(&mut self, output: &str, reason: impl std::fmt::Display) {
        let key = self.aliases.get(output).map_or(output, String::as_str);
        if let Some(entry) = self.entries.get_mut(key) {
            entry.candidate = None;
            entry.error = Some(format!(
                "Wallpaper frame failed; previous image retained: {reason}"
            ));
            self.changed();
        }
    }
}

fn supports_r8(renderer: &mut GlesRenderer) -> bool {
    // R8 is required, not an RGBA emulation or ES2 luminance approximation.
    #[allow(unsafe_code)]
    renderer
        .with_context(|gl| unsafe {
            let version = gl.GetString(ffi::VERSION);
            !version.is_null()
                && std::ffi::CStr::from_ptr(version.cast())
                    .to_bytes()
                    .windows(11)
                    .any(|s| s == b"OpenGL ES 3")
        })
        .unwrap_or(false)
}
fn import(
    renderer: &mut GlesRenderer,
    image: &WallpaperImage,
    mapping: &RasterMapping,
    programs: &mut ContextPrograms,
) -> Result<Imported, String> {
    let size = (
        i32::try_from(image.raster.width).map_err(|_| "Texture width overflow")?,
        i32::try_from(image.raster.height).map_err(|_| "Texture height overflow")?,
    );
    #[allow(unsafe_code)]
    let limit = renderer
        .with_context(|gl| unsafe {
            let mut max = 0;
            gl.GetIntegerv(ffi::MAX_TEXTURE_SIZE, &mut max);
            max
        })
        .map_err(|e| e.to_string())?;
    if size.0 > limit || size.1 > limit {
        return Err(format!("Wallpaper texture exceeds GLES limit {limit}"));
    }
    let indexed = image.raster.format == PixelFormat::Indexed8;
    let r8 = programs.r8;
    if indexed && !r8 {
        return Err("This GLES context lacks a real R8 indexed wallpaper path".into());
    }
    // Compile each format only once per context, before allocating candidate textures.
    let program = programs.program(renderer, indexed)?;
    let (texture, palette) = if indexed {
        let texture = upload_r8(renderer, mapping.as_bytes(), size)?;
        let mut colors = [0u8; 1024];
        for (destination, color) in colors.chunks_exact_mut(4).zip(&image.raster.palette) {
            destination.copy_from_slice(color);
        }
        let palette = renderer
            .import_memory(&colors, Fourcc::Abgr8888, (256, 1).into(), false)
            .map_err(|e| e.to_string())?;
        (texture, Some(palette))
    } else {
        (
            renderer
                .import_memory(mapping.as_bytes(), Fourcc::Abgr8888, size.into(), false)
                .map_err(|e| e.to_string())?,
            None,
        )
    };
    // Pixel shaders bypass Smithay's RGBA/external texture-format dispatch, which cannot
    // represent R8. The texture remains genuinely GL_R8; no format lie or RGBA expansion.
    #[allow(unsafe_code)]
    renderer
        .with_context(|gl| unsafe {
            let (mut active, mut bound) = (0, 0);
            gl.GetIntegerv(ffi::ACTIVE_TEXTURE, &mut active);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut bound);
            gl.BindTexture(ffi::TEXTURE_2D, texture.tex_id());
            let filter = if indexed { ffi::NEAREST } else { ffi::LINEAR } as i32;
            let wrap =
                if r8 && !indexed && image.sizing == templar_types::wallpaper::SizingMode::Tile {
                    ffi::REPEAT
                } else {
                    ffi::CLAMP_TO_EDGE
                } as i32;
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, filter);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, filter);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, wrap);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, wrap);
            if let Some(palette) = &palette {
                gl.BindTexture(ffi::TEXTURE_2D, palette.tex_id());
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_MIN_FILTER,
                    ffi::NEAREST as i32,
                );
                gl.TexParameteri(
                    ffi::TEXTURE_2D,
                    ffi::TEXTURE_MAG_FILTER,
                    ffi::NEAREST as i32,
                );
            }
            gl.BindTexture(ffi::TEXTURE_2D, bound as u32);
            gl.ActiveTexture(active as u32);
        })
        .map_err(|error| error.to_string())?;
    #[allow(unsafe_code)]
    let gl_error = renderer
        .with_context(|gl| unsafe { gl.GetError() })
        .map_err(|error| error.to_string())?;
    if gl_error != ffi::NO_ERROR {
        return Err(format!(
            "Wallpaper GLES allocation/shader error 0x{gl_error:x}"
        ));
    }
    Ok(Imported {
        image: image.clone(),
        texture,
        palette,
        program,
        context: renderer.context_id(),
        id: Id::new(),
        ground_id: Id::new(),
    })
}
#[allow(unsafe_code)]
fn upload_r8(
    renderer: &mut GlesRenderer,
    bytes: &[u8],
    size: (i32, i32),
) -> Result<GlesTexture, String> {
    let id = renderer
        .with_context(|gl| unsafe {
            let mut alignment = 4;
            let mut bound = 0;
            gl.GetIntegerv(ffi::UNPACK_ALIGNMENT, &mut alignment);
            gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut bound);
            let mut id = 0;
            gl.GenTextures(1, &mut id);
            gl.BindTexture(ffi::TEXTURE_2D, id);
            gl.PixelStorei(ffi::UNPACK_ALIGNMENT, 1);
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_MIN_FILTER,
                ffi::NEAREST as i32,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_MAG_FILTER,
                ffi::NEAREST as i32,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_S,
                ffi::CLAMP_TO_EDGE as i32,
            );
            gl.TexParameteri(
                ffi::TEXTURE_2D,
                ffi::TEXTURE_WRAP_T,
                ffi::CLAMP_TO_EDGE as i32,
            );
            gl.TexImage2D(
                ffi::TEXTURE_2D,
                0,
                ffi::R8 as i32,
                size.0,
                size.1,
                0,
                ffi::RED,
                ffi::UNSIGNED_BYTE,
                bytes.as_ptr().cast(),
            );
            let error = gl.GetError();
            gl.PixelStorei(ffi::UNPACK_ALIGNMENT, alignment);
            gl.BindTexture(ffi::TEXTURE_2D, bound as u32);
            if error != ffi::NO_ERROR {
                gl.DeleteTextures(1, &id);
                Err(format!("GLES R8 allocation/upload error 0x{error:x}"))
            } else {
                Ok(id)
            }
        })
        .map_err(|e| e.to_string())??;
    // SAFETY: The successful context-local upload created this texture; ownership transfers once
    // to Smithay's context-bound deferred cleanup queue, including candidate rollback/unplug.
    Ok(unsafe { GlesTexture::from_raw(renderer, Some(ffi::R8), true, id, size.into()) })
}

const RGBA_SHADER: &str = r#"
precision highp float;
varying vec2 v_coords;
uniform sampler2D tex;
uniform float alpha;
uniform vec2 wall_size;
uniform float wall_tile;
#ifdef DEBUG_FLAGS
uniform float tint;
#endif
void main() {
 vec2 uv = wall_tile > 0.5 ? fract(v_coords) : v_coords;
 gl_FragColor = texture2D(tex, uv) * alpha;
}
"#;
const INDEX_SHADER: &str = r#"
precision highp float;
varying vec2 v_coords;
uniform sampler2D tex;
uniform sampler2D wall_palette;
uniform vec2 wall_size;
uniform float wall_tile;
uniform float alpha;
#ifdef DEBUG_FLAGS
uniform float tint;
#endif
vec4 resolve(vec2 pixel) {
 if (wall_tile > 0.5) pixel = mod(pixel, wall_size);
 else pixel = clamp(pixel, vec2(0.0), wall_size - vec2(1.0));
 float index = floor(texture2D(tex, (pixel + vec2(0.5)) / wall_size).r * 255.0 + 0.5);
 return texture2D(wall_palette, vec2((index + 0.5) / 256.0, 0.5));
}
void main() {
 vec2 pixel = v_coords * wall_size - vec2(0.5);
 vec2 base = floor(pixel); vec2 weight = fract(pixel);
 // Resolve FOUR exact indices first; interpolation is in color space, never index space.
 gl_FragColor = mix(mix(resolve(base), resolve(base + vec2(1.0,0.0)), weight.x),
                    mix(resolve(base + vec2(0.0,1.0)), resolve(base + vec2(1.0,1.0)), weight.x), weight.y) * alpha;
}
"#;

#[derive(Debug)]
pub struct WallpaperElement {
    imported: Arc<Imported>,
    geometry: Rectangle<i32, Physical>,
    source: Rectangle<f64, Buffer>,
    tile: bool,
}
impl Element for WallpaperElement {
    fn id(&self) -> &Id {
        &self.imported.id
    }
    fn current_commit(&self) -> CommitCounter {
        CommitCounter::default()
    }
    fn geometry(&self, _: Scale<f64>) -> Rectangle<i32, Physical> {
        self.geometry
    }
    fn src(&self) -> Rectangle<f64, Buffer> {
        self.source
    }
    fn opaque_regions(&self, _: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::from_slice(&[Rectangle::from_size(self.geometry.size)])
    }
    fn alpha(&self) -> f32 {
        1.0
    }
    fn kind(&self) -> Kind {
        Kind::Unspecified
    }
}
impl WallpaperElement {
    fn draw_gles(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque: &[Rectangle<i32, Physical>],
    ) -> Result<(), GlesError> {
        let indexed = self.imported.palette.is_some();
        let uniforms = [
            Uniform::new("tex", 0i32),
            Uniform::new(
                "wall_size",
                (
                    self.imported.texture.width() as f32,
                    self.imported.texture.height() as f32,
                ),
            ),
            Uniform::new("wall_tile", if self.tile { 1.0f32 } else { 0.0f32 }),
            Uniform::new("wall_palette", 1i32),
        ];
        let (mut previous_zero, mut previous_one, mut active) = (0, 0, 0);
        #[allow(unsafe_code)]
        frame.with_context(|gl| unsafe {
            gl.GetIntegerv(ffi::ACTIVE_TEXTURE, &mut active);
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut previous_zero);
            gl.BindTexture(ffi::TEXTURE_2D, self.imported.texture.tex_id());
            if let Some(palette) = &self.imported.palette {
                gl.ActiveTexture(ffi::TEXTURE1);
                gl.GetIntegerv(ffi::TEXTURE_BINDING_2D, &mut previous_one);
                gl.BindTexture(ffi::TEXTURE_2D, palette.tex_id());
            }
            gl.ActiveTexture(ffi::TEXTURE0);
        })?;
        let result = frame.render_pixel_shader_to(
            &self.imported.program,
            src,
            dst,
            self.imported.texture.size(),
            Some(damage),
            1.0,
            &uniforms[..if indexed { 4 } else { 3 }],
        );
        #[allow(unsafe_code)]
        frame.with_context(|gl| unsafe {
            gl.ActiveTexture(ffi::TEXTURE0);
            gl.BindTexture(ffi::TEXTURE_2D, previous_zero as u32);
            if indexed {
                gl.ActiveTexture(ffi::TEXTURE1);
                gl.BindTexture(ffi::TEXTURE_2D, previous_one as u32);
            }
            gl.ActiveTexture(active as u32);
        })?;
        result
    }
}
impl<R: WallpaperRenderer> RenderElement<R> for WallpaperElement {
    fn draw(
        &self,
        frame: &mut R::Frame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque: &[Rectangle<i32, Physical>],
        _: Option<&UserDataMap>,
    ) -> Result<(), R::Error> {
        R::draw_wallpaper(self, frame, src, dst, damage, opaque)
    }
}
#[derive(Debug)]
pub enum WallpaperScene {
    Image(WallpaperElement),
    Ground(SolidColorRenderElement),
}
fn make_elements(imported: Arc<Imported>, output: &Output) -> Vec<WallpaperScene> {
    let Some(physical) = crate::viewport::visible_physical_size(output) else {
        return Vec::new();
    };
    let raster = &imported.image.raster;
    let Ok(layout) = templar_wallpaper::geometry::layout(
        (raster.source_width, raster.source_height),
        (physical.w as u32, physical.h as u32),
        imported.image.sizing,
    ) else {
        return Vec::new();
    };
    let [x, y, w, h] = layout.destination;
    let mut uv = layout.source;
    if let Some([period_x, period_y]) = layout.tile {
        uv = [0.0, 0.0, w / f64::from(period_x), h / f64::from(period_y)];
    }
    let source = Rectangle::new(
        (
            uv[0] * f64::from(raster.width),
            uv[1] * f64::from(raster.height),
        )
            .into(),
        (
            uv[2] * f64::from(raster.width),
            uv[3] * f64::from(raster.height),
        )
            .into(),
    );
    let ground = SolidColorRenderElement::new(
        imported.ground_id.clone(),
        Rectangle::from_size(physical),
        CommitCounter::default(),
        imported.image.ground.map(|c| f32::from(c) / 255.0),
        Kind::Unspecified,
    );
    vec![
        WallpaperScene::Image(WallpaperElement {
            tile: layout.tile.is_some(),
            imported,
            geometry: Rectangle::new(
                (x.round() as i32, y.round() as i32).into(),
                (w.round() as i32, h.round() as i32).into(),
            ),
            source,
        }),
        WallpaperScene::Ground(ground),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    fn candidate(revision: Revision, budget: u64) -> (WallpaperImage, RasterMapping) {
        use std::io::Write;
        let mut file = templar_wallpaper::raster::create_memfd().unwrap();
        file.write_all(&[255, 0, 0, 255]).unwrap();
        templar_wallpaper::raster::seal(&file).unwrap();
        let image = WallpaperImage {
            revision,
            display_revision: Revision::new_owner(),
            output: DisplayId("gpu/connector/edid".into()),
            raster: templar_types::wallpaper::RasterDescriptor {
                version: 1,
                width: 1,
                height: 1,
                source_width: 1,
                source_height: 1,
                stride: 4,
                format: PixelFormat::Rgba8,
                palette: Vec::new(),
                bytes: 4,
            },
            sizing: templar_types::wallpaper::SizingMode::Fill,
            ground: [0, 0, 0, 255],
            transition_budget_bytes: budget,
        };
        let mapping = RasterMapping::new(&file, &image.raster).unwrap();
        (image, mapping)
    }
    #[test]
    fn descriptor_acceptance_does_not_attest_frame_presentation() {
        let mut state = WallpaperState::default();
        let job = Revision::new_owner().at_generation(5);
        let (image, mapping) = candidate(job, 8);
        state.accept(image, mapping).unwrap();
        let observed = state.observation();
        assert_eq!(observed.outputs[0].accepted, Some(job));
        assert_eq!(observed.outputs[0].presented, None);
        assert_eq!(observed.outputs[0].source_bytes, 4);
        assert_eq!(observed.outputs[0].texture_bytes, 0);
        assert!(!observed.active());
        state.frame_presented("gpu/connector/edid", Some(job));
        assert!(
            !state.observation().active(),
            "An unimported raster is never a presented frame"
        );
    }
    #[test]
    fn latest_pending_replaces_old_mapping_and_stale_jobs_are_rejected() {
        let mut state = WallpaperState::default();
        let job = Revision::new_owner();
        for generation in [2, 3] {
            let (image, mapping) = candidate(job.at_generation(generation), 8);
            state.accept(image, mapping).unwrap();
            assert_eq!(state.observation().outputs[0].source_bytes, 4);
        }
        let (image, mapping) = candidate(job.at_generation(1), 8);
        assert!(state.accept(image, mapping).is_err());
        assert_eq!(
            state.observation().outputs[0].accepted,
            Some(job.at_generation(3))
        );
        assert!(state.clear(job.at_generation(2), None).is_err());
        state.clear(job.at_generation(3), None).unwrap();
        assert!(state.observation().outputs.is_empty());
    }
    #[test]
    fn transition_budget_counts_backing_and_candidate_texture_before_acceptance() {
        let mut state = WallpaperState::default();
        let (image, mapping) = candidate(Revision::new_owner(), 7);
        assert!(
            state
                .accept(image, mapping)
                .unwrap_err()
                .contains("8 bytes")
        );
        assert!(state.observation().outputs.is_empty());
    }
    #[test]
    fn observation_serial_changes_only_when_state_changes_and_unplug_drops_mapping() {
        let mut state = WallpaperState::default();
        state.bind_outputs(vec![(
            "HDMI-A-1".into(),
            DisplayId("gpu/connector/edid".into()),
        )]);
        assert_eq!(state.canonical("HDMI-A-1"), "gpu/connector/edid");
        let (image, mapping) = candidate(Revision::new_owner(), 8);
        state.accept(image, mapping).unwrap();
        let revision = state.observation().revision;
        state.publish();
        assert_eq!(state.observation().revision, revision);
        state.reject(
            &DisplayId("gpu/connector/edid".into()),
            "candidate error".into(),
        );
        assert!(state.observation().revision.generation > revision.generation);
        assert_eq!(state.observation().outputs[0].source_bytes, 4);
        state.retain_outputs(&[]);
        assert!(state.observation().outputs.is_empty());
    }
}

#[cfg(test)]
mod topology_tests {
    use super::*;
    #[test]
    fn topology_fence_drops_only_candidate_and_reports_actionable_redecode() {
        use std::io::Write;
        let mut state = WallpaperState::default();
        let topology = Revision::new_owner();
        let mut file = templar_wallpaper::raster::create_memfd().unwrap();
        file.write_all(&[1, 2, 3, 255]).unwrap();
        templar_wallpaper::raster::seal(&file).unwrap();
        let raster = templar_types::wallpaper::RasterDescriptor {
            version: 1,
            width: 1,
            height: 1,
            source_width: 1,
            source_height: 1,
            stride: 4,
            format: PixelFormat::Rgba8,
            palette: Vec::new(),
            bytes: 4,
        };
        let mapping = RasterMapping::new(&file, &raster).unwrap();
        state
            .accept(
                WallpaperImage {
                    revision: Revision::new_owner(),
                    display_revision: topology,
                    output: DisplayId("winit".into()),
                    raster,
                    sizing: templar_types::wallpaper::SizingMode::Fill,
                    ground: [0, 0, 0, 255],
                    transition_budget_bytes: 8,
                },
                mapping,
            )
            .unwrap();
        state.fence_topology(topology.at_generation(1));
        let output = &state.observation().outputs[0];
        assert_eq!(output.source_bytes, 0);
        assert_eq!(output.texture_bytes, 0);
        assert_eq!(output.presented, None);
        assert!(output.error.as_ref().unwrap().contains("re-decode"));
    }
}

#[cfg(test)]
mod clear_fence_tests {
    use super::*;
    #[test]
    fn clear_keeps_owner_watermark_after_all_output_resources_are_removed() {
        use std::io::Write;
        let mut state = WallpaperState::default();
        let owner = Revision::new_owner();
        state.clear(owner.at_generation(9), None).unwrap();
        assert!(state.observation().outputs.is_empty());
        let mut file = templar_wallpaper::raster::create_memfd().unwrap();
        file.write_all(&[1, 2, 3, 255]).unwrap();
        templar_wallpaper::raster::seal(&file).unwrap();
        let raster = templar_types::wallpaper::RasterDescriptor {
            version: 1,
            width: 1,
            height: 1,
            source_width: 1,
            source_height: 1,
            stride: 4,
            format: PixelFormat::Rgba8,
            palette: Vec::new(),
            bytes: 4,
        };
        let image = WallpaperImage {
            revision: owner.at_generation(8),
            display_revision: Revision::new_owner(),
            output: DisplayId("winit".into()),
            raster: raster.clone(),
            sizing: templar_types::wallpaper::SizingMode::Fill,
            ground: [0, 0, 0, 255],
            transition_budget_bytes: 8,
        };
        assert!(
            state
                .accept(image.clone(), RasterMapping::new(&file, &raster).unwrap())
                .is_err()
        );
        assert!(
            state.observation().outputs.is_empty(),
            "A late mapped FD cannot resurrect cleared resources"
        );
        assert!(state.clear(owner.at_generation(8), None).is_err());
        let current = WallpaperImage {
            revision: owner.at_generation(10),
            ..image
        };
        state
            .accept(current.clone(), RasterMapping::new(&file, &raster).unwrap())
            .unwrap();
        let another_output = WallpaperImage {
            output: DisplayId("second".into()),
            transition_budget_bytes: 16,
            ..current
        };
        state
            .accept(another_output, RasterMapping::new(&file, &raster).unwrap())
            .unwrap();
        assert_eq!(
            state.observation().outputs.len(),
            2,
            "One job may target multiple outputs at the same revision"
        );
    }
}
