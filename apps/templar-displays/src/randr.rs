//! Real X11 RandR configuration/readback. It never attests native KMS or protected frames.
use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use std::{collections::{BTreeMap, BTreeSet}, time::{Duration, Instant}};
use templar_types::{display::{self, Display, DisplayId, DisplayMode, DisplayObservation, DisplayProfile, Insets, OutputConfiguration, PhysicalState, Rotation}, settings::{Capability, Revision}};
use uuid::Uuid;
use x11rb::{connection::Connection as _, protocol::{randr, render, shape, xproto::{self, ConnectionExt as _}}, rust_connection::RustConnection, CURRENT_TIME, COPY_DEPTH_FROM_PARENT};

#[derive(Clone, Debug)]
struct CrtcState { info: randr::GetCrtcInfoReply, transform: render::Transform, filter: Vec<u8>, params: Vec<render::Fixed>, has_transforms: bool }
#[derive(Clone, Debug)]
struct Borders { mode: u32, horizontal: u32, vertical: u32, on: u32, off: u32, value: u32, h: u32, v: u32, h_range: (u32,u32), v_range: (u32,u32) }
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub observation: DisplayObservation,
    pub fingerprint: [u8; 32],
    resources: randr::GetScreenResourcesCurrentReply,
    outputs: BTreeMap<u32, randr::GetOutputInfoReply>,
    crtcs: BTreeMap<u32, CrtcState>,
    borders: BTreeMap<u32, Borders>,
    geometry: xproto::GetGeometryReply,
    primary: u32,
}
pub struct Randr {
    connection: RustConnection, root: u32, screen: usize, owner: Uuid, generation: u64,
    last: Option<[u8;32]>, intent: Option<DisplayProfile>, overlays: Vec<u32>, overlay_deadline: Option<Instant>,
}
impl Randr {
    pub fn connect(owner: Uuid, generation: u64) -> Result<Self> {
        let (connection, screen) = x11rb::connect(None).context("Connect to the actual X11 server from DISPLAY/XAUTHORITY")?;
        let root = connection.setup().roots[screen].root;
        let version = randr::query_version(&connection, 1, 5)?.reply()?;
        ensure!((version.major_version, version.minor_version) >= (1,3), "The X server lacks RandR 1.3 configuration/transforms");
        randr::select_input(&connection,root,randr::NotifyMask::SCREEN_CHANGE | randr::NotifyMask::CRTC_CHANGE | randr::NotifyMask::OUTPUT_CHANGE | randr::NotifyMask::OUTPUT_PROPERTY)?.check()?;
        Ok(Self { connection, root, screen, owner, generation, last: None, intent: None, overlays: Vec::new(), overlay_deadline: None })
    }
    pub fn adopt(&mut self, observation: &DisplayObservation, fingerprint: [u8;32]) { self.generation = observation.revision.generation; self.last = Some(fingerprint); self.intent = Some(observation.profile.clone()); }
    pub fn changed(&mut self)->Result<bool> {
        let expired=self.overlay_deadline.is_some_and(|deadline|Instant::now()>=deadline);
        self.expire_identification()?;
        let mut changed=expired;
        while let Some(event)=self.connection.poll_for_event()? { changed |= matches!(event,x11rb::protocol::Event::RandrNotify(_) | x11rb::protocol::Event::RandrScreenChangeNotify(_)); }
        Ok(changed)
    }
    fn atom(&self, name: &[u8]) -> Result<u32> { Ok(self.connection.intern_atom(true, name)?.reply()?.atom) }
    fn property(&self, output: u32, atom: u32) -> Result<Option<randr::GetOutputPropertyReply>> {
        if atom == 0 { return Ok(None); }
        let reply = randr::get_output_property(&self.connection, output, atom, xproto::AtomEnum::ANY, 0, 16384, false, false)?.reply()?;
        ensure!(reply.bytes_after == 0, "RandR output property exceeds the bounded read");
        Ok((reply.type_ != 0).then_some(reply))
    }
    fn property32(&self, output: u32, atom: u32, kind: xproto::AtomEnum) -> Result<u32> {
        let reply = self.property(output, atom)?.context("Output property is absent")?;
        ensure!(reply.format == 32 && reply.type_ == u32::from(kind) && reply.data.len() == 4, "Output property has an unsupported native type");
        Ok(u32::from_ne_bytes(reply.data.try_into().expect("length checked")))
    }
    fn borders(&self, output: u32) -> Result<Option<Borders>> {
        let mode = self.atom(b"underscan")?;
        if mode == 0 || self.property(output, mode)?.is_none() { return Ok(None); }
        let horizontal = self.atom(b"underscan hborder")?; let vertical = self.atom(b"underscan vborder")?;
        let advertised = randr::query_output_property(&self.connection, output, mode)?.reply()?;
        ensure!(!advertised.range, "Hardware underscan is not an advertised enum");
        let mut on = None; let mut off = None;
        for value in advertised.valid_values { let value = u32::try_from(value).context("Invalid hardware enum atom")?; match self.connection.get_atom_name(value)?.reply()?.name.as_slice() { b"on" => on = Some(value), b"off" => off = Some(value), _ => {} } }
        fn range(connection: &RustConnection, output: u32, atom: u32) -> Result<(u32,u32)> { ensure!(atom != 0, "Border property absent"); let range = randr::query_output_property(connection, output, atom)?.reply()?; ensure!(range.range && range.valid_values.len() == 2, "Border range is not advertised"); Ok((u32::try_from(range.valid_values[0])?, u32::try_from(range.valid_values[1])?)) }
        Ok(Some(Borders { mode, horizontal, vertical, on: on.context("Hardware on enum absent")?, off: off.context("Hardware off enum absent")?, value: self.property32(output, mode, xproto::AtomEnum::ATOM)?, h: self.property32(output, horizontal, xproto::AtomEnum::INTEGER)?, v: self.property32(output, vertical, xproto::AtomEnum::INTEGER)?, h_range: range(&self.connection, output, horizontal)?, v_range: range(&self.connection, output, vertical)? }))
    }
    pub fn snapshot(&mut self) -> Result<Snapshot> {
        self.expire_identification()?;
        let resources = randr::get_screen_resources_current(&self.connection, self.root)?.reply()?;
        let geometry = self.connection.get_geometry(self.root)?.reply()?;
        let primary = randr::get_output_primary(&self.connection, self.root)?.reply()?.output;
        let mut crtcs = BTreeMap::new();
        for crtc in &resources.crtcs { let info = randr::get_crtc_info(&self.connection, *crtc, resources.config_timestamp)?.reply()?; ensure!(info.status == randr::SetConfig::SUCCESS, "CRTC inventory changed while reading"); let transform = randr::get_crtc_transform(&self.connection, *crtc)?.reply()?; crtcs.insert(*crtc, CrtcState { info, transform: transform.current_transform, filter: transform.current_filter_name, params: transform.current_params, has_transforms: transform.has_transforms }); }
        let mut outputs = BTreeMap::new(); let mut displays = Vec::new(); let mut borders = BTreeMap::new(); let mut fingerprint = Sha256::new();
        fingerprint.update(resources.timestamp.to_ne_bytes()); fingerprint.update(resources.config_timestamp.to_ne_bytes()); fingerprint.update(primary.to_ne_bytes()); fingerprint.update(geometry.width.to_ne_bytes()); fingerprint.update(geometry.height.to_ne_bytes());
        for (crtc,state) in &crtcs {fingerprint_crtc(&mut fingerprint,*crtc,state);}
        for output in &resources.outputs {
            let info = randr::get_output_info(&self.connection, *output, resources.config_timestamp)?.reply()?; ensure!(info.status == randr::SetConfig::SUCCESS, "Output inventory changed while reading");
            let connector = String::from_utf8(info.name.clone()).context("The X server advertises a non-UTF8 connector name")?;
            let edid = self.property(*output, self.atom(b"EDID")?)?.filter(|reply| reply.format == 8).map(|reply| reply.data);
            let digest = edid.as_ref().filter(|bytes| valid_edid(bytes)).map(|bytes| format!("{:x}", Sha256::digest(bytes)));
            let gpu = physical_gpu(&connector, digest.as_deref());
            if let Some(digest)=&digest {fingerprint.update(digest.as_bytes());}
            if let Some(gpu)=&gpu {fingerprint.update(gpu.as_bytes());}
            let id = match (&gpu, &digest) { (Some(gpu), Some(digest)) => DisplayId(format!("{gpu}/{connector}/{digest}")), _ => DisplayId(format!("temporary:x11:{}/{output}", self.owner)) };
            let persistence = if gpu.is_some() && digest.is_some() { available() } else { Capability::Unavailable { reason: "RandR output cannot be mapped uniquely to a stable kernel GPU/connector/EDID; configuration is session-only".into() } };
            let mut modes: Vec<_> = info.modes.iter().enumerate().filter_map(|(index, id)| resources.modes.iter().find(|mode| mode.id == *id).map(|mode| display_mode(mode, index < usize::from(info.num_preferred)))).collect(); modes.dedup_by(|a,b| a.id == b.id);
            let border_result = self.borders(*output);
            let hardware_underscan = match &border_result { Ok(Some(_)) => available(), Ok(None) => Capability::Unsupported { reason: "The actual RandR driver does not advertise hardware underscan".into() }, Err(error) => Capability::Unavailable { reason: format!("Hardware underscan properties cannot be interpreted: {error:#}") } };
            let inset = border_result.as_ref().ok().and_then(Option::as_ref).filter(|border| border.value == border.on).map_or(Insets::default(), |border| Insets::symmetric(border.h, border.v));
            if let Ok(Some(border)) = border_result { fingerprint.update(border.value.to_ne_bytes()); fingerprint.update(border.h.to_ne_bytes()); fingerprint.update(border.v.to_ne_bytes()); borders.insert(*output, border); }
            let crtc = crtcs.get(&info.crtc).filter(|crtc| crtc.info.mode != 0);
            let mode = crtc.and_then(|crtc| resources.modes.iter().find(|mode| mode.id == crtc.info.mode));
            let rotation = crtc.map_or(Ok(Rotation::Normal), |crtc| rotation(crtc.info.rotation));
            let scale = match (crtc, mode) { (Some(crtc), Some(mode)) => scale_from_transform(&crtc.transform, mode, inset, rotation.as_ref().copied().unwrap_or(Rotation::Normal)), _ => Ok(1.0) };
            let configure = if rotation.is_err() || scale.is_err() { Capability::Unavailable { reason: "Existing reflection/shear/perspective or anisotropic transform cannot be represented by this display editor; raw rollback still retains it".into() } } else if info.crtcs.is_empty() || modes.is_empty() { Capability::Unavailable { reason: "The X server advertises no usable CRTC route or exact signal timings".into() } } else { available() };
            let mut current = OutputConfiguration { id: id.clone(), enabled: crtc.is_some(), mode: mode.map(mode_id), scale: scale.unwrap_or(1.0), rotation: rotation.unwrap_or(Rotation::Normal), x: crtc.map_or(0, |crtc| i32::from(crtc.info.x)), y: crtc.map_or(0, |crtc| i32::from(crtc.info.y)), mirror_of: None, primary: *output == primary, insets: inset };
            if !current.enabled { if let Some(intent) = self.intent.as_ref().and_then(|profile| profile.outputs.iter().find(|configuration| configuration.id == id)) { current = intent.clone(); current.enabled = false; current.primary = false; } }
            let metadata = edid.as_deref().map(edid_metadata).unwrap_or_default();
            let physical = PhysicalState::RandRObserved { enabled: current.enabled, mode: mode.map(mode_id) };
            fingerprint.update(output.to_ne_bytes()); fingerprint.update(info.name.as_slice()); fingerprint.update(u8::from(info.connection).to_ne_bytes()); fingerprint.update(info.crtc.to_ne_bytes());
            fingerprint.update(info.crtcs.len().to_ne_bytes()); for crtc in &info.crtcs {fingerprint.update(crtc.to_ne_bytes());}
            for mode in &modes { fingerprint.update(mode.id.as_bytes()); }
            displays.push(Display { id, gpu: gpu.unwrap_or_else(|| format!("X11 server {}", self.screen)), connector: connector.clone(), make: metadata.0, model: metadata.1, serial: metadata.2, edid_sha256: digest, connected: info.connection == randr::Connection::CONNECTED, internal: connector.starts_with("eDP") || connector.starts_with("LVDS") || connector.starts_with("DSI"), modes, current, physical, persistence, configure, hardware_underscan, composition_inset: Capability::Unsupported { reason: "RandR does not own final desktop composition/black margins; asymmetric compositor calibration and protected physical-frame proof require the native compositor".into() } });
            outputs.insert(*output, info);
        }
        displays.sort_by(|a,b| a.id.cmp(&b.id));
        let fence = randr::get_screen_resources_current(&self.connection, self.root)?.reply()?;
        ensure!(fence.timestamp == resources.timestamp && fence.config_timestamp == resources.config_timestamp && fence.outputs == resources.outputs && fence.crtcs == resources.crtcs, "RandR topology/configuration changed during the inventory read; no mixed-generation snapshot published");
        let primary_id = displays.iter().find(|display| display.connected && display.current.enabled && display.current.primary).map(|display| display.id.clone());
        for display in &mut displays { display.current.primary = primary_id.as_ref() == Some(&display.id); }
        // X11 has one root framebuffer: exactly equal rectangles genuinely show the same region.
        for index in 0..displays.len() { if !displays[index].current.enabled { continue; } let source = (0..index).find(|other| displays[*other].current.enabled && displays[*other].current.mirror_of.is_none() && same_region(&displays[*other], &displays[index])); if let Some(source) = source { displays[index].current.mirror_of = Some(displays[source].id.clone()); } }
        let fingerprint: [u8;32] = fingerprint.finalize().into();
        if self.last.is_some_and(|old| old != fingerprint) { self.generation = self.generation.checked_add(1).context("RandR generation exhausted")?; }
        self.last = Some(fingerprint);
        let profile = DisplayProfile { outputs: displays.iter().filter(|display| display.connected).map(|display| display.current.clone()).collect(), allow_headless: !displays.iter().any(|display| display.connected && display.current.enabled) };
        let can_identify = if !self.connection.query_extension(b"SHAPE")?.reply()?.present {
            Capability::Unsupported { reason: "Input-transparent actual identification needs the X11 Shape extension".into() }
        } else if self.connection.list_fonts(1,b"fixed")?.reply()?.names.is_empty() {
            Capability::Unavailable { reason: "The actual X server does not advertise the core fixed font used for connector identification".into() }
        } else { available() };
        let observation = DisplayObservation { revision: Revision { owner_epoch: self.owner, generation: self.generation }, displays, profile, preview: None, failures: Vec::new(), session_active: true, locked: false, panel_closed: false, can_identify };
        Ok(Snapshot { observation, fingerprint, resources, outputs, crtcs, borders, geometry, primary })
    }
    pub fn validate(&self, snapshot: &Snapshot, expected: Revision, profile: &DisplayProfile) -> Result<Vec<(u32,u32,u32)>> {
        display::validate_profile(&snapshot.observation, expected, profile)?;
        let range = randr::get_screen_size_range(&self.connection, self.root)?.reply()?;
        let mut used = BTreeMap::new(); let mut candidates = Vec::new();
        for configuration in &profile.outputs { self.output_id(snapshot, &configuration.id)?; }
        for configuration in profile.outputs.iter().filter(|configuration| configuration.enabled) {
            ensure!(configuration.x >= 0 && configuration.y >= 0 && configuration.x <= i32::from(i16::MAX) && configuration.y <= i32::from(i16::MAX), "RandR CRTC coordinates must be nonnegative and fit signed 16-bit positions");
            let output = self.output_id(snapshot, &configuration.id)?; let info = &snapshot.outputs[&output];
            let mode = info.modes.iter().find(|id| snapshot.resources.modes.iter().any(|mode| mode.id == **id && configuration.mode.as_ref().is_some_and(|id| *id == mode_id(mode)))).copied().context("Exact advertised RandR timing disappeared")?;
            let insets = configuration.insets;
            if !insets.is_zero() { let border = snapshot.borders.get(&output).context("The driver has no usable hardware underscan")?; ensure!(insets.left == insets.right && insets.top == insets.bottom, "Asymmetric borders need final compositor composition; RandR cannot implement them"); ensure!((border.h_range.0..=border.h_range.1).contains(&insets.left) && (border.v_range.0..=border.v_range.1).contains(&insets.top), "Requested physical borders are outside the actual driver ranges"); }
            let mode_info=snapshot.resources.modes.iter().find(|candidate|candidate.id==mode).expect("advertised timing");
            let transform=target_transform(mode_info,configuration)?;
            let advertised = info.crtcs.iter().copied().filter(|crtc| snapshot.crtcs.get(crtc).is_some_and(|state| (u16::from(state.info.rotations) & u16::from(randr_rotation(configuration.rotation))) != 0 && supports_transform(state,&transform))).collect::<Vec<_>>();
            ensure!(!advertised.is_empty(), "The actual server has no route supporting this rotation/transform; unchanged transforms do not require driver transform support"); candidates.push((output, mode, advertised));
        }
        fn assign(index: usize, candidates: &[(u32,u32,Vec<u32>)], used: &mut BTreeMap<u32,usize>, seen: &mut BTreeSet<u32>) -> bool { for crtc in &candidates[index].2 { if !seen.insert(*crtc) { continue; } if used.get(crtc).copied().is_none_or(|previous| assign(previous, candidates, used, seen)) { used.insert(*crtc, index); return true; } } false }
        for index in 0..candidates.len() { ensure!(assign(index, &candidates, &mut used, &mut BTreeSet::new()), "The actual RandR outputs cannot be assigned usable distinct CRTCs"); }
        let (width,height) = framebuffer_size(snapshot, profile, (range.min_width, range.min_height))?;
        ensure!(width >= range.min_width && width <= range.max_width && height >= range.min_height && height <= range.max_height, "Draft exceeds the actual X server framebuffer limits");
        candidates.into_iter().enumerate().map(|(index,(output,mode,_))| Ok((output, *used.iter().find(|(_,owner)| **owner == index).context("CRTC assignment lost")?.0, mode))).collect()
    }
    fn output_id(&self, snapshot: &Snapshot, id: &DisplayId) -> Result<u32> {
        let display = snapshot.observation.displays.iter().find(|display| &display.id == id).context("Output identity disappeared")?;
        let mut matches = snapshot.outputs.iter().filter(|(_,info)| info.name == display.connector.as_bytes());
        let output = *matches.next().context("Output disappeared")?.0;
        ensure!(matches.next().is_none(), "Actual connector name is ambiguous; refusing a name-based output guess");
        Ok(output)
    }
    pub fn apply(&mut self, snapshot: &Snapshot, expected: Revision, profile: &DisplayProfile) -> Result<Snapshot> {
        let plans = self.validate(snapshot, expected, profile)?;
        self.connection.grab_server()?.check()?;
        let result = (|| {
        let current = self.snapshot()?; ensure!(current.fingerprint == snapshot.fingerprint && current.observation.revision == expected, "RandR owner/configuration changed before effects");
        for crtc in snapshot.resources.crtcs.iter().filter(|crtc| snapshot.crtcs.get(crtc).is_some_and(|state| state.info.mode != 0)) { self.configure_crtc(*crtc, 0, 0, 0, randr::Rotation::ROTATE0, &[])?; }
        let range = randr::get_screen_size_range(&self.connection, self.root)?.reply()?;
        let (width,height) = framebuffer_size(snapshot, profile, (range.min_width, range.min_height))?; self.set_screen_size(width,height)?;
        for (output,crtc,mode) in plans {
            let id = snapshot.observation.displays.iter().find(|display| snapshot.outputs[&output].name == display.connector.as_bytes()).context("Planned output disappeared")?.id.clone();
            let configuration = profile.outputs.iter().find(|configuration| configuration.id == id).expect("validated plan");
            self.set_borders(snapshot, output, configuration.insets)?;
            let mode_info = snapshot.resources.modes.iter().find(|candidate| candidate.id == mode).expect("advertised timing");
            let transform=target_transform(mode_info,configuration)?;
            if snapshot.crtcs[&crtc].transform!=transform {randr::set_crtc_transform(&self.connection, crtc, transform, b"bilinear", &[])?.check()?;}
            self.configure_crtc(crtc, configuration.x as i16, configuration.y as i16, mode, randr_rotation(configuration.rotation), &[output])?;
        }
        let primary = profile.outputs.iter().find(|output| output.enabled && output.primary).map(|output| self.output_id(snapshot, &output.id)).transpose()?.unwrap_or(0);
        randr::set_output_primary(&self.connection, self.root, primary)?.check()?; self.connection.flush()?; self.intent = Some(profile.clone());
        let applied = self.snapshot()?; self.verify(&applied, profile)?; Ok(applied)
        })();
        let released = self.connection.ungrab_server()?.check();
        self.connection.flush()?;
        released?;
        result
    }
    fn configure_crtc(&self, crtc: u32, x: i16, y: i16, mode: u32, rotation: randr::Rotation, outputs: &[u32]) -> Result<()> { let resources = randr::get_screen_resources_current(&self.connection, self.root)?.reply()?; let reply = randr::set_crtc_config(&self.connection, crtc, CURRENT_TIME, resources.config_timestamp, x, y, mode, rotation, outputs)?.reply()?; ensure!(reply.status == randr::SetConfig::SUCCESS, "The real RandR server rejected CRTC configuration: {:?}", reply.status); Ok(()) }
    fn set_screen_size(&self, width: u16, height: u16) -> Result<()> { let screen = &self.connection.setup().roots[self.screen]; let mmw = (u64::from(screen.width_in_millimeters) * u64::from(width) / u64::from(screen.width_in_pixels.max(1))).max(1) as u32; let mmh = (u64::from(screen.height_in_millimeters) * u64::from(height) / u64::from(screen.height_in_pixels.max(1))).max(1) as u32; randr::set_screen_size(&self.connection, self.root, width, height, mmw, mmh)?.check()?; Ok(()) }
    fn write_property32(&self, output:u32, atom:u32, kind:xproto::AtomEnum, value:u32) -> Result<()> { randr::change_output_property(&self.connection, output, atom, kind.into(), 32, xproto::PropMode::REPLACE, 1, &value.to_ne_bytes())?.check()?; Ok(()) }
    fn set_borders(&self, snapshot:&Snapshot, output:u32, insets:Insets) -> Result<()> { if let Some(border) = snapshot.borders.get(&output) { self.write_property32(output, border.horizontal, xproto::AtomEnum::INTEGER, if insets.is_zero() { border.h_range.0 } else { insets.left })?; self.write_property32(output, border.vertical, xproto::AtomEnum::INTEGER, if insets.is_zero() { border.v_range.0 } else { insets.top })?; self.write_property32(output, border.mode, xproto::AtomEnum::ATOM, if insets.is_zero() { border.off } else { border.on })?; } else { ensure!(insets.is_zero(), "Hardware borders unavailable"); } Ok(()) }
    pub fn verify(&self, snapshot: &Snapshot, target: &DisplayProfile) -> Result<()> {
        ensure!(display::matches_connected_inventory(target, &snapshot.observation.displays), "Connected identity set changed during the RandR transaction");
        let expected_primary = target.outputs.iter().find(|output| output.enabled && output.primary).map(|output| self.output_id(snapshot,&output.id)).transpose()?.unwrap_or(0);
        ensure!(snapshot.primary == expected_primary, "Actual X server primary differs from the complete requested layout");
        for configuration in &target.outputs {
            let output = self.output_id(snapshot, &configuration.id)?; let info = &snapshot.outputs[&output]; let crtc = snapshot.crtcs.get(&info.crtc).filter(|state| state.info.mode != 0);
            if !configuration.enabled { ensure!(crtc.is_none(), "Requested disabled RandR output remains configured"); continue; }
            let crtc = crtc.context("Requested enabled RandR output has no actual CRTC")?; let mode = snapshot.resources.modes.iter().find(|mode| mode.id == crtc.info.mode).context("Actual CRTC mode absent")?;
            ensure!(configuration.mode.as_ref().is_some_and(|id| *id == mode_id(mode)) && i32::from(crtc.info.x) == configuration.x && i32::from(crtc.info.y) == configuration.y && crtc.info.rotation == randr_rotation(configuration.rotation) && crtc.transform == target_transform(mode, configuration)?, "Actual X server mode/position/rotation/16.16 transform differs from the target");
            if let Some(border) = snapshot.borders.get(&output) { ensure!(border.value == if configuration.insets.is_zero() { border.off } else { border.on }, "Actual hardware underscan enum differs from target"); if !configuration.insets.is_zero() { ensure!(border.h == configuration.insets.left && border.v == configuration.insets.top, "Actual hardware borders differ from target"); } }
        }
        let range = randr::get_screen_size_range(&self.connection, self.root)?.reply()?;
        let (width,height) = framebuffer_size(snapshot,target,(range.min_width,range.min_height))?;
        ensure!(snapshot.geometry.width == width && snapshot.geometry.height == height, "Actual root framebuffer differs from the complete requested desktop");
        Ok(())
    }
    pub fn rollback(&mut self, baseline: &Snapshot) -> Result<Snapshot> {
        self.connection.grab_server()?.check()?;
        let result = (|| {
            let current = self.snapshot()?;
            let connected: BTreeSet<_> = current.outputs.iter().filter(|(_,info)| info.connection == randr::Connection::CONNECTED).map(|(output,_)| *output).collect();
            let surviving: BTreeSet<_> = current.observation.displays.iter().filter(|display| display.connected && baseline.observation.displays.iter().any(|old| old.connected && old.id == display.id)).map(|display| self.output_id(&current,&display.id)).collect::<Result<_>>()?;
            let new_crtcs: BTreeSet<_> = current.crtcs.iter().filter(|(_,state)| state.info.outputs.iter().any(|output| connected.contains(output) && !surviving.contains(output))).map(|(crtc,_)| *crtc).collect();
            for (crtc,state) in &baseline.crtcs {
                if state.info.mode != 0 && state.info.outputs.iter().any(|output| surviving.contains(output)) {
                    ensure!(!new_crtcs.contains(crtc), "A newly attached head occupies an original CRTC; rollback retains ownership rather than disabling that new head");
                    let original=baseline.resources.modes.iter().find(|mode| mode.id==state.info.mode).context("Original raw timing is absent")?;
                    ensure!(current.resources.modes.iter().any(|mode| mode.id==original.id && mode_id(mode)==mode_id(original)), "Original exact RandR timing disappeared; rollback cannot invent a replacement");
                }
            }
            for crtc in &current.resources.crtcs { if baseline.crtcs.contains_key(crtc) && !new_crtcs.contains(crtc) { self.configure_crtc(*crtc,0,0,0,randr::Rotation::ROTATE0,&[])?; } }
            let (width,height) = if new_crtcs.is_empty() {(baseline.geometry.width,baseline.geometry.height)}else{(baseline.geometry.width.max(current.geometry.width),baseline.geometry.height.max(current.geometry.height))};
            self.set_screen_size(width,height)?;
            for (output,border) in &baseline.borders { if surviving.contains(output) { self.write_property32(*output,border.horizontal,xproto::AtomEnum::INTEGER,border.h)?; self.write_property32(*output,border.vertical,xproto::AtomEnum::INTEGER,border.v)?; self.write_property32(*output,border.mode,xproto::AtomEnum::ATOM,border.value)?; } }
            for (crtc,state) in &baseline.crtcs {
                if !current.resources.crtcs.contains(crtc) || new_crtcs.contains(crtc) { continue; }
                let outputs: Vec<_> = state.info.outputs.iter().filter(|output| surviving.contains(output)).copied().collect();
                if state.info.mode != 0 && !outputs.is_empty() {
                    let actual=&current.crtcs[crtc];
                    if actual.transform!=state.transform || actual.filter!=state.filter || actual.params!=state.params {randr::set_crtc_transform(&self.connection,*crtc,state.transform,&state.filter,&state.params)?.check()?;}
                    self.configure_crtc(*crtc,state.info.x,state.info.y,state.info.mode,state.info.rotation,&outputs)?;
                }
            }
            let primary = if surviving.contains(&baseline.primary) { baseline.primary } else if new_crtcs.is_empty() { 0 } else { current.primary };
            randr::set_output_primary(&self.connection,self.root,primary)?.check()?; self.connection.flush()?; self.intent = Some(baseline.observation.profile.clone());
            let restored = self.snapshot()?;
            ensure!(restored.geometry.width == width && restored.geometry.height == height && restored.primary == primary, "Original root framebuffer/primary failed actual rollback readback");
            for (crtc,before) in &baseline.crtcs {
                if !current.resources.crtcs.contains(crtc) || new_crtcs.contains(crtc) { continue; }
                let expected_outputs: BTreeSet<_> = before.info.outputs.iter().filter(|output| surviving.contains(output)).copied().collect();
                let actual = restored.crtcs.get(crtc).context("Restored CRTC disappeared")?;
                if before.info.mode == 0 || expected_outputs.is_empty() { ensure!(actual.info.mode == 0 && actual.info.outputs.is_empty(), "Originally disabled/disconnected output remained enabled after rollback"); continue; }
                ensure!(actual.info.mode == before.info.mode && actual.info.x == before.info.x && actual.info.y == before.info.y && actual.info.rotation == before.info.rotation && actual.transform == before.transform && actual.filter == before.filter && actual.params == before.params && actual.info.outputs.iter().copied().collect::<BTreeSet<_>>() == expected_outputs, "Original raw CRTC configuration failed rollback readback");
            }
            for (output,before) in &baseline.borders { if surviving.contains(output) { let actual=restored.borders.get(output).context("Original hardware border properties disappeared")?; ensure!(actual.value==before.value && actual.h==before.h && actual.v==before.v,"Original hardware borders failed rollback readback"); } }
            for crtc in &new_crtcs { let before=&current.crtcs[crtc]; let actual=restored.crtcs.get(crtc).context("New-head CRTC disappeared during rollback")?; ensure!(actual.info.mode==before.info.mode && actual.info.x==before.info.x && actual.info.y==before.info.y && actual.info.rotation==before.info.rotation && actual.info.outputs==before.info.outputs && actual.transform==before.transform && actual.filter==before.filter && actual.params==before.params,"Rollback disturbed a genuinely new head"); }
            Ok(restored)
        })();
        let released=self.connection.ungrab_server()?.check(); self.connection.flush()?; released?; result
    }
    pub fn identify(&mut self, snapshot: &Snapshot, ids: &[DisplayId], calibration: bool) -> Result<()> {
        ensure!(matches!(snapshot.observation.can_identify, Capability::Available { .. }), "Input-transparent actual identification unavailable");
        self.clear_identification()?;
        let result = (|| {
            for id in ids {
                let display=snapshot.observation.displays.iter().find(|display| &display.id==id && display.current.enabled).context("Identify needs an actually enabled output")?;
                let mode=display.modes.iter().find(|mode| display.current.mode.as_ref()==Some(&mode.id)).context("Identify mode unavailable")?;
                let (width,height)=display::logical_size(mode,&display.current)?;
                ensure!(width>=4 && height>=4 && width<=i32::from(i16::MAX) && height<=i32::from(i16::MAX),"Actual identification geometry exceeds X11 drawing coordinates");
                let window=self.connection.generate_id()?;
                self.connection.create_window(COPY_DEPTH_FROM_PARENT,window,self.root,display.current.x.try_into()?,display.current.y.try_into()?,width.try_into()?,height.try_into()?,0,xproto::WindowClass::INPUT_OUTPUT,0,&xproto::CreateWindowAux::new().override_redirect(1).background_pixel(0x00182e55))?.check()?;
                self.overlays.push(window);
                shape::rectangles(&self.connection,shape::SO::SET,shape::SK::INPUT,xproto::ClipOrdering::UNSORTED,window,0,0,&[])?.check()?;
                if calibration {
                    let reach=(width.min(height)/8).clamp(4,100);
                    let mut marks=Vec::with_capacity(11);
                    for (x,y) in [(0,0),(width-reach,0),(0,height-reach),(width-reach,height-reach)] {
                        marks.push(xproto::Rectangle {x:x.try_into()?,y:if y==0{0}else{(height-3).try_into()?},width:reach.try_into()?,height:3});
                        marks.push(xproto::Rectangle {x:if x==0{0}else{(width-3).try_into()?},y:y.try_into()?,width:3,height:reach.try_into()?});
                    }
                    marks.push(xproto::Rectangle {x:(width/2-12).max(0).try_into()?,y:(height/2).try_into()?,width:25.min(width).try_into()?,height:1});
                    marks.push(xproto::Rectangle {x:(width/2).try_into()?,y:(height/2-12).max(0).try_into()?,width:1,height:25.min(height).try_into()?});
                    marks.push(xproto::Rectangle {x:0,y:0,width:width.try_into()?,height:36.min(height).try_into()?});
                    shape::rectangles(&self.connection,shape::SO::SET,shape::SK::BOUNDING,xproto::ClipOrdering::UNSORTED,window,0,0,&marks)?.check()?;
                }
                let font=self.connection.generate_id()?;
                let gc=self.connection.generate_id()?;
                self.connection.open_font(font,b"fixed")?.check()?;
                let painted=(|| {
                    self.connection.create_gc(gc,window,&xproto::CreateGCAux::new().foreground(0x00ffffff).line_width(3).font(font))?.check()?;
                    let drawn=(|| {
                        self.connection.map_window(window)?.check()?;
                        self.connection.poly_rectangle(window,gc,&[xproto::Rectangle {x:1,y:1,width:(width-3).try_into()?,height:(height-3).try_into()?}])?.check()?;
                        let number=snapshot.observation.displays.iter().filter(|display|display.connected).position(|display| &display.id==id).context("Identify inventory changed")?+1;
                        let label=format!("{number}: {} - X server view, not KMS proof",display.connector);
                        self.connection.image_text8(window,gc,4,24,&label.as_bytes()[..label.len().min(255)])?.check()?;
                        if calibration {self.connection.poly_segment(window,gc,&[xproto::Segment {x1:0,y1:(height/2).try_into()?,x2:width.try_into()?,y2:(height/2).try_into()?},xproto::Segment {x1:(width/2).try_into()?,y1:0,x2:(width/2).try_into()?,y2:height.try_into()?}])?.check()?;}
                        Ok::<_,anyhow::Error>(())
                    })();
                    let freed=self.connection.free_gc(gc)?.check();
                    drawn?; freed?; Ok::<_,anyhow::Error>(())
                })();
                let closed=self.connection.close_font(font)?.check();
                painted?; closed?;
            }
            self.connection.flush()?;
            self.overlay_deadline=Some(Instant::now()+Duration::from_secs(if calibration{30}else{5}));
            Ok(())
        })();
        if result.is_err() {if let Err(cleanup)=self.clear_identification(){return result.context(format!("Identification teardown also failed: {cleanup:#}"));}}
        result
    }
    pub fn expire_identification(&mut self) -> Result<()> { if self.overlay_deadline.is_some_and(|deadline| Instant::now() >= deadline) { self.clear_identification()?; } Ok(()) }
    fn clear_identification(&mut self) -> Result<()> {
        let mut failure=None; let mut retained=Vec::new();
        for window in std::mem::take(&mut self.overlays) {
            let result=(||->Result<()>{self.connection.destroy_window(window)?.check()?;Ok(())})();
            if let Err(error)=result {retained.push(window);if failure.is_none(){failure=Some(error);}}
        }
        self.overlays=retained;
        if let Some(error)=failure {return Err(error).context("Exact owned identification teardown failed; unremoved window identities retained");}
        self.overlay_deadline=None; Ok(())
    }
}
fn available() -> Capability { Capability::Available { requires_restart:false } }
fn mode_id(mode:&randr::ModeInfo) -> String { format!("randr:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",mode.width,mode.height,mode.dot_clock,mode.hsync_start,mode.hsync_end,mode.htotal,mode.hskew,mode.vsync_start,mode.vsync_end,mode.vtotal,u32::from(mode.mode_flags)) }
fn display_mode(mode:&randr::ModeInfo,preferred:bool) -> DisplayMode { let flags=u32::from(mode.mode_flags); let mut refresh=u64::from(mode.dot_clock)*1000/u64::from(mode.htotal.max(1))/u64::from(mode.vtotal.max(1)); if flags & u32::from(randr::ModeFlag::INTERLACE)!=0 { refresh*=2; } if flags & u32::from(randr::ModeFlag::DOUBLE_SCAN)!=0 { refresh/=2; } DisplayMode { id:mode_id(mode),width:u32::from(mode.width),height:u32::from(mode.height),refresh_millihz:refresh.min(u64::from(u32::MAX)) as u32,preferred,interlaced:flags & u32::from(randr::ModeFlag::INTERLACE)!=0 } }
fn rotation(rotation:randr::Rotation) -> Result<Rotation> { match rotation { randr::Rotation::ROTATE0=>Ok(Rotation::Normal),randr::Rotation::ROTATE90=>Ok(Rotation::Left),randr::Rotation::ROTATE180=>Ok(Rotation::Inverted),randr::Rotation::ROTATE270=>Ok(Rotation::Right),_=>bail!("Reflection or unknown rotation cannot be represented") } }
fn randr_rotation(rotation:Rotation) -> randr::Rotation { match rotation { Rotation::Normal=>randr::Rotation::ROTATE0,Rotation::Left=>randr::Rotation::ROTATE90,Rotation::Inverted=>randr::Rotation::ROTATE180,Rotation::Right=>randr::Rotation::ROTATE270 } }
fn transform_values(transform:&render::Transform)->[i32;9] { [transform.matrix11,transform.matrix12,transform.matrix13,transform.matrix21,transform.matrix22,transform.matrix23,transform.matrix31,transform.matrix32,transform.matrix33] }
fn fingerprint_crtc(fingerprint:&mut Sha256,crtc:u32,state:&CrtcState) {
    fingerprint.update(crtc.to_ne_bytes()); fingerprint.update(state.info.mode.to_ne_bytes());
    fingerprint.update(state.info.x.to_ne_bytes()); fingerprint.update(state.info.y.to_ne_bytes());
    fingerprint.update(u16::from(state.info.rotation).to_ne_bytes()); fingerprint.update(u16::from(state.info.rotations).to_ne_bytes());
    fingerprint.update(state.info.outputs.len().to_ne_bytes()); for output in &state.info.outputs {fingerprint.update(output.to_ne_bytes());}
    fingerprint.update(u8::from(state.has_transforms).to_ne_bytes()); for value in transform_values(&state.transform) {fingerprint.update(value.to_ne_bytes());}
    fingerprint.update(state.filter.len().to_ne_bytes()); fingerprint.update(&state.filter);
    fingerprint.update(state.params.len().to_ne_bytes()); for value in &state.params {fingerprint.update(value.to_ne_bytes());}
}
fn supports_transform(state:&CrtcState,transform:&render::Transform)->bool {state.has_transforms || state.transform==*transform}
fn target_transform(mode:&randr::ModeInfo,configuration:&OutputConfiguration)->Result<render::Transform> { let (visible_w,visible_h)=configuration.insets.visible_size(u32::from(mode.width),u32::from(mode.height)).context("Margins leave no usable signal")?; let (width,height,visible_w,visible_h)=if matches!(configuration.rotation,Rotation::Left|Rotation::Right) {(mode.height,mode.width,visible_h,visible_w)}else{(mode.width,mode.height,visible_w,visible_h)}; let x=(f64::from(visible_w)/f64::from(width)/configuration.scale*65536.0).round(); let y=(f64::from(visible_h)/f64::from(height)/configuration.scale*65536.0).round(); ensure!(x>0.0 && y>0.0 && x<=f64::from(i32::MAX) && y<=f64::from(i32::MAX),"Requested transform cannot be represented in RandR 16.16"); Ok(render::Transform { matrix11:x as i32,matrix12:0,matrix13:0,matrix21:0,matrix22:y as i32,matrix23:0,matrix31:0,matrix32:0,matrix33:65536 }) }
fn scale_from_transform(transform:&render::Transform,mode:&randr::ModeInfo,insets:Insets,rotation:Rotation)->Result<f64> { let values=transform_values(transform); ensure!(values[1]==0 && values[2]==0 && values[3]==0 && values[5]==0 && values[6]==0 && values[7]==0 && values[8]==65536 && values[0]>0 && values[4]>0,"Non-axis-aligned existing transform"); let (w,h)=insets.visible_size(u32::from(mode.width),u32::from(mode.height)).context("Invalid hardware borders")?; let (mw,mh,w,h)=if matches!(rotation,Rotation::Left|Rotation::Right) {(mode.height,mode.width,h,w)}else{(mode.width,mode.height,w,h)}; let sx=f64::from(w)/f64::from(mw)*65536.0/f64::from(values[0]); let sy=f64::from(h)/f64::from(mh)*65536.0/f64::from(values[4]); ensure!((sx-sy).abs()<0.0002 && sx.is_finite() && (0.5..=4.0).contains(&sx),"Existing transform is not a supported uniform scale"); Ok(sx) }
fn framebuffer_size(snapshot:&Snapshot,profile:&DisplayProfile,minimum:(u16,u16))->Result<(u16,u16)> { let mut width=i64::from(minimum.0.max(1)); let mut height=i64::from(minimum.1.max(1)); for configuration in profile.outputs.iter().filter(|output|output.enabled) { let display=snapshot.observation.displays.iter().find(|display|display.id==configuration.id).context("Unknown display")?; let mode=display.modes.iter().find(|mode|configuration.mode.as_ref()==Some(&mode.id)).context("Unknown exact mode")?; let (w,h)=display::logical_size(mode,configuration)?; width=width.max(i64::from(configuration.x)+i64::from(w)); height=height.max(i64::from(configuration.y)+i64::from(h)); } Ok((width.try_into()?,height.try_into()?)) }
fn same_region(a:&Display,b:&Display)->bool { let size=|display:&Display|display.modes.iter().find(|mode|display.current.mode.as_ref()==Some(&mode.id)).and_then(|mode|display::logical_size(mode,&display.current).ok()); a.current.x==b.current.x && a.current.y==b.current.y && size(a).is_some() && size(a)==size(b) }
fn valid_edid(bytes:&[u8])->bool { bytes.len()>=128 && bytes[..8]==[0,255,255,255,255,255,255,0] && bytes[..128].iter().fold(0_u8,|sum,byte|sum.wrapping_add(*byte))==0 }
fn physical_gpu(connector:&str,digest:Option<&str>)->Option<String> {
    use std::io::Read as _;
    let digest=digest?; let mut found=None; let suffix=format!("-{connector}");
    for entry in std::fs::read_dir("/sys/class/drm").ok()? {
        let entry=entry.ok()?; let name=entry.file_name(); let name=name.to_str()?;
        let Some(card)=name.strip_suffix(&suffix) else {continue;};
        if !card.strip_prefix("card").is_some_and(|number|number.parse::<u32>().is_ok()) {continue;}
        let mut bytes=Vec::new(); std::fs::File::open(entry.path().join("edid")).ok()?.take(32769).read_to_end(&mut bytes).ok()?;
        if bytes.len()>32768 || format!("{:x}",Sha256::digest(&bytes))!=digest {continue;}
        let gpu=std::fs::canonicalize(format!("/sys/class/drm/{card}/device")).ok()?.to_str()?.strip_prefix("/sys/")?.to_owned();
        if found.is_some() {return None;} found=Some(gpu);
    }
    found
}
fn edid_metadata(bytes:&[u8])->(String,String,Option<String>) { if !valid_edid(bytes) {return Default::default();} let vendor=u16::from_be_bytes([bytes[8],bytes[9]]); let make=[((vendor>>10)&31) as u8,((vendor>>5)&31) as u8,(vendor&31) as u8].map(|value|char::from(value.saturating_add(b'A'-1))).into_iter().collect(); let mut model=String::new(); let mut serial=None; for descriptor in bytes[54..126].chunks_exact(18) { if descriptor[..3]==[0,0,0] { let text=String::from_utf8_lossy(&descriptor[5..18]).trim_matches(|character| matches!(character,'\0'|'\n'|'\r'|' ')).to_owned(); match descriptor[3] { 0xfc=>model=text,0xff if !text.is_empty()=>serial=Some(text),_=>{} } } } (make,model,serial) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn exact_timing_identity_includes_sync_and_flags() { let mut mode=randr::ModeInfo::default(); mode.width=1920; mode.height=1080; mode.dot_clock=148500000; mode.htotal=2200; mode.vtotal=1125; let original=mode_id(&mode); mode.hsync_start=2008; assert_ne!(original,mode_id(&mode)); let with_sync=mode_id(&mode); mode.mode_flags=randr::ModeFlag::INTERLACE; assert_ne!(with_sync,mode_id(&mode)); }
    #[test] fn signal_borders_change_source_transform_not_advertised_mode() { let mode=randr::ModeInfo {width:1920,height:1080,..Default::default()}; let configuration=OutputConfiguration {id:DisplayId("head".into()),enabled:true,mode:None,scale:1.0,rotation:Rotation::Normal,x:0,y:0,mirror_of:None,primary:true,insets:Insets::symmetric(20,10)}; let transform=target_transform(&mode,&configuration).unwrap(); assert!(transform.matrix11<65536); assert!(transform.matrix22<65536); assert_eq!((mode.width,mode.height),(1920,1080)); }
    #[test] fn driver_without_transforms_accepts_only_unchanged_raw_transform() {
        let identity=render::Transform {matrix11:65536,matrix22:65536,matrix33:65536,..Default::default()};
        let mut state=CrtcState {info:randr::GetCrtcInfoReply::default(),transform:identity,filter:Vec::new(),params:Vec::new(),has_transforms:false};
        assert!(supports_transform(&state,&identity));
        let scaled=render::Transform {matrix11:32768,matrix22:32768,..identity};
        assert!(!supports_transform(&state,&scaled));
        state.has_transforms=true;
        assert!(supports_transform(&state,&scaled));
    }
    #[test] fn exact_generation_includes_raw_filter_parameters_and_disabled_crtc_transform() {
        let identity=render::Transform {matrix11:65536,matrix22:65536,matrix33:65536,..Default::default()};
        let baseline=CrtcState {info:randr::GetCrtcInfoReply::default(),transform:identity,filter:Vec::new(),params:Vec::new(),has_transforms:false};
        let digest=|state:&CrtcState| {let mut fingerprint=Sha256::new();fingerprint_crtc(&mut fingerprint,7,state);fingerprint.finalize()};
        let original=digest(&baseline);
        let mut changed=baseline.clone();changed.filter=b"nearest".to_vec();assert_ne!(original,digest(&changed));
        let mut changed=baseline.clone();changed.params.push(65536);assert_ne!(original,digest(&changed));
        let mut changed=baseline.clone();changed.transform.matrix11=32768;assert_eq!(changed.info.mode,0);assert_ne!(original,digest(&changed));
        let mut changed=baseline.clone();changed.info.rotations=randr::Rotation::ROTATE0;assert_ne!(original,digest(&changed));
    }
    #[test]
    #[ignore = "requires DISPLAY and TEMPLAR_DISPLAYS_OWNED_TEST_DISPLAY to name the same privately owned Xvfb"]
    fn unchanged_transform_primary_transaction_restores_actual_xvfb() {
        let owned=std::env::var("TEMPLAR_DISPLAYS_OWNED_TEST_DISPLAY").expect("Explicit private Xvfb ownership is required");
        assert_eq!(std::env::var("DISPLAY").unwrap(),owned);
        let mut backend=Randr::connect(Uuid::new_v4(),0).unwrap();
        let baseline=backend.snapshot().unwrap();
        assert_eq!(baseline.observation.displays.len(),1);
        assert_eq!(baseline.observation.displays[0].connector,"screen");
        assert!(baseline.crtcs.values().all(|state|!state.has_transforms));
        let mut target=baseline.observation.profile.clone();
        target.outputs[0].primary=!target.outputs[0].primary;
        let applied=backend.apply(&baseline,baseline.observation.revision,&target);
        let restored=backend.rollback(&baseline).expect("The raw unchanged transform must not require unsupported Xvfb effects");
        assert_eq!(applied.unwrap().observation.profile,target);
        assert_eq!(restored.observation.profile,baseline.observation.profile);
    }
}
