use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt}, path::PathBuf};
use templar_display_ui::{DisplaysCommand, DisplaysState};
use templar_types::{display::{self, DisplayObservation, DisplayProfile}, settings::Capability};
use uuid::Uuid;
use crate::{preview::Preview, randr::Randr};
const LIMIT: u64 = 1024*1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Preferences { version: u32, profiles: Vec<DisplayProfile> }
fn preferences_path()->Result<PathBuf> {
    let home=std::env::var_os("HOME").map(PathBuf::from).context("HOME is required for explicit durable display profiles")?;
    let base=std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(||home.join(".config")); ensure!(base.is_absolute(),"Configuration directory must be absolute"); Ok(base.join("templar-displays/profiles.json"))
}
fn preferences()->Result<Preferences> {
    use std::io::Read as _;
    let path=preferences_path()?;
    let file=match fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(&path) {Ok(file)=>file,Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return Ok(Preferences {version:1,profiles:Vec::new()}),Err(error)=>return Err(error).context("Read durable display preferences without following a final symlink")};
    let metadata=file.metadata()?; let uid=fs::metadata("/proc/self")?.uid();
    ensure!(metadata.is_file() && metadata.uid()==uid && metadata.mode()&0o022==0 && metadata.len()<=LIMIT,"Display preferences are not a bounded, exclusively user-owned regular file");
    let mut bytes=Vec::new(); file.take(LIMIT+1).read_to_end(&mut bytes)?; ensure!(bytes.len() as u64<=LIMIT,"Display preference size changed beyond the bound");
    let preferences:Preferences=serde_json::from_slice(&bytes)?; ensure!(preferences.version==1,"Unsupported standalone display preference version"); Ok(preferences)
}
fn save(observation:&DisplayObservation)->Result<()> {
    ensure!(observation.displays.iter().filter(|display|display.connected).all(|display|matches!(display.persistence,Capability::Available {..})),"The X server configuration was confirmed, but this topology has temporary identities and cannot be saved durably");
    let mut preferences=preferences()?; preferences.profiles.retain(|profile|!display::matches_connected_inventory(profile,&observation.displays)); preferences.profiles.push(observation.profile.clone());
    let bytes=serde_json::to_vec(&preferences)?; ensure!(bytes.len() as u64<=LIMIT,"Confirmed profile inventory exceeds the bounded durable preference format; existing file left unchanged");
    let path=preferences_path()?; let directory=path.parent().context("Preference directory missing")?;
    let uid=fs::metadata("/proc/self")?.uid();
    match fs::symlink_metadata(directory) {Ok(metadata)=>ensure!(metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.uid()==uid && metadata.mode()&0o022==0,"Owned display preference directory must be exclusively user-owned and not a symlink"),Err(error) if error.kind()==std::io::ErrorKind::NotFound=>{fs::create_dir_all(directory)?;fs::set_permissions(directory,fs::Permissions::from_mode(0o700))?;},Err(error)=>return Err(error.into())}
    if let Ok(metadata)=fs::symlink_metadata(&path) {ensure!(metadata.is_file() && !metadata.file_type().is_symlink(),"Refuse to replace a nonregular or symlinked display preference file");}
    let mut temporary=tempfile::NamedTempFile::new_in(directory)?; temporary.as_file().set_permissions(fs::Permissions::from_mode(0o600))?; temporary.write_all(&bytes)?; temporary.as_file().sync_all()?; temporary.persist(&path).map_err(|error|error.error)?; fs::File::open(directory)?.sync_all()?; Ok(())
}
const SOURCE: &str="Standalone X11: actual RandR server configuration/readback only. No native KMS TEST_ONLY, physical page-flip proof, protected lock barrier, or compositor-only asymmetric black margins. RandR transforms use 16.16 precision; readback shows their actual resulting scale.";
pub struct Controller { backend: Randr, preview: Option<Preview>, topology: Option<Vec<templar_types::display::DisplayId>> }
impl Controller {
    pub fn new()->Result<Self> {Ok(Self {backend:Randr::connect(Uuid::new_v4(),0)?,preview:None,topology:None})}
    pub fn changed(&mut self)->Result<bool> {self.backend.changed()}
    pub fn run(&mut self,command:DisplaysCommand)->DisplaysState {
        let operation=self.operation(command);
        if self.preview.as_ref().is_some_and(Preview::finished) { self.preview.take(); }
        let observation=self.actual();
        match observation {Ok((observation,read_issue))=>DisplaysState {observation:Some(observation),message:Some(match (operation,read_issue) {(Ok(()),None)=>SOURCE.into(),(Ok(()),Some(issue))=>format!("{issue} · {SOURCE}"),(Err(error),issue)=>format!("{error:#}{} · {SOURCE}",issue.map_or(String::new(),|issue|format!("; {issue}")))})},Err(read)=>DisplaysState {observation:None,message:Some(match operation {Ok(())=>format!("Actual X server state unavailable: {read:#}"),Err(write)=>format!("Display operation failed: {write:#}; actual readback unavailable: {read:#}")})}}
    }
    fn actual(&mut self)->Result<(DisplayObservation,Option<String>)> {
        if let Some(preview)=&mut self.preview {
            match preview.observe() {
                Ok((observation,fingerprint,finished))=>{self.backend.adopt(&observation,fingerprint);if finished{self.preview.take();}return Ok((observation,None));},
                Err(error) if preview.finished()=>{self.preview.take();return Ok((self.backend.snapshot()?.observation,Some(format!("Owned preview ended without a usable final reply: {error:#}; actual X server state was read, not assumed restored"))));},
                Err(error)=>return Err(error),
            }
        }
        Ok((self.backend.snapshot()?.observation,None))
    }
    fn operation(&mut self,command:DisplaysCommand)->Result<()> {
        match command {
            DisplaysCommand::Refresh=>{
                if self.preview.is_some() { return Ok(()); }
                let actual=self.backend.snapshot()?; let ids:Vec<_>=actual.observation.displays.iter().filter(|display|display.connected).map(|display|display.id.clone()).collect();
                let changed=self.topology.as_ref()!=Some(&ids); self.topology=Some(ids);
                if changed && self.preview.is_none() {if let Some(profile)=preferences()?.profiles.into_iter().find(|profile|display::matches_connected_inventory(profile,&actual.observation.displays)) {
                    if profile!=actual.observation.profile && actual.observation.displays.iter().filter(|display|display.connected).all(|display|matches!(display.persistence,Capability::Available {..})) {
                        let (mut preview,applied,fingerprint)=Preview::begin(&actual,profile)?; self.backend.adopt(&applied,fingerprint);
                        let (confirmed,fingerprint,finished)=preview.confirm(applied.revision,preview.token)?; ensure!(finished,"Trusted stored profile did not complete actual server readback"); self.backend.adopt(&confirmed,fingerprint);
                    }
                }}
            },
            DisplaysCommand::Validate {expected,profile}=>{let actual=self.backend.snapshot()?; self.backend.validate(&actual,expected,&profile)?;},
            DisplaysCommand::Preview {expected,profile}=>{ensure!(self.preview.is_none(),"Confirm or revert the current independently owned preview first"); let actual=self.backend.snapshot()?; ensure!(actual.observation.revision==expected,"The actual output owner/topology changed"); let (preview,observation,fingerprint)=Preview::begin(&actual,profile)?; self.backend.adopt(&observation,fingerprint); self.preview=Some(preview);},
            DisplaysCommand::Confirm {expected,token}=>{let preview=self.preview.as_mut().context("No independently owned preview")?; let (observation,fingerprint,finished)=preview.confirm(expected,token)?; ensure!(finished,"The rollback owner did not confirm this exact server generation"); self.backend.adopt(&observation,fingerprint); self.preview.take(); save(&observation)?;},
            DisplaysCommand::Revert {token}=>{let preview=self.preview.as_mut().context("No independently owned preview")?; let (observation,fingerprint,finished)=preview.revert(token)?; self.backend.adopt(&observation,fingerprint); if finished {self.preview.take();}},
            DisplaysCommand::Identify {expected,outputs,calibration}=>{let actual=self.backend.snapshot()?; ensure!(actual.observation.revision==expected,"The output owner/configuration changed before identification"); self.backend.identify(&actual,&outputs,calibration)?;},
        }
        Ok(())
    }
}
