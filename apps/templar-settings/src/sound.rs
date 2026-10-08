//! Sound reads and selects owned Audio1 devices; stable preferences are separate from defaults.
use templar_client::{proxies::AudioProxy, status::{AudioDirection, AudioInventory, AudioState}};

#[derive(Clone, Debug, Default)]
pub struct SoundState {
    pub inventory: Option<AudioInventory>,
    pub output: Option<AudioState>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub enum SoundCommand {
    Refresh,
    Select { direction: AudioDirection, name: String },
    Step(i32),
    SetVolume(u32),
    ToggleMute,
}

pub async fn run(command: SoundCommand) -> SoundState {
    let result = async {
        let connection = zbus::Connection::session().await?;
        let proxy = AudioProxy::new(&connection).await?;
        let write = match command {
            SoundCommand::Refresh => Ok(()),
            SoundCommand::Select { direction, name } => proxy.select_device(
                match direction { AudioDirection::Output => "output", AudioDirection::Input => "input" }, &name
            ).await,
            SoundCommand::Step(delta) => proxy.adjust_volume(delta).await.map(|_| ()),
            SoundCommand::ToggleMute => proxy.toggle_muted().await.map(|_| ()),
            SoundCommand::SetVolume(percent) => proxy.set_volume(percent).await,
        };
        // Read both even after a refused write. No desired-state assignment.
        let (inventory, output) = tokio::join!(proxy.inventory(), proxy.get());
        let mut errors = Vec::new();
        if let Err(error) = write { errors.push(error.to_string()); }
        let inventory = inventory.map_err(anyhow::Error::from).and_then(|json|
            serde_json::from_str(&json).map_err(Into::into)).map_err(|error| errors.push(error.to_string())).ok();
        let output: Option<AudioState> = output.map_err(anyhow::Error::from).and_then(|json|
            serde_json::from_str(&json).map_err(Into::into)).map_err(|error| errors.push(error.to_string())).ok();
        if let Some(reason) = output.as_ref().and_then(|state| state.unavailable_reason.as_ref()) {
            errors.push(reason.clone());
        }
        Ok::<_, anyhow::Error>(SoundState { inventory, output,
            error: (!errors.is_empty()).then(|| errors.join("; ")) })
    }.await;
    result.unwrap_or_else(|error| SoundState { error: Some(error.to_string()), ..SoundState::default() })
}

pub async fn render() -> String {
    let state = run(SoundCommand::Refresh).await;
    let mut lines = vec!["Sound".to_owned()];
    if let Some(error) = state.error { lines.push(format!("Unavailable: {error}")); }
    if let Some(output) = state.output { lines.push(format!("Output: {}", output.headline())); }
    if let Some(inventory) = state.inventory {
        for device in inventory.devices {
            lines.push(format!("{:?}: {} [{}]{}", device.direction, device.description,
                device.name, if device.is_default { " (applied default)" } else { "" }));
        }
        if let Some(name) = inventory.preferred_output { lines.push(format!("Saved output preference: {name}")); }
        if let Some(name) = inventory.preferred_input { lines.push(format!("Saved input preference: {name}")); }
    }
    lines.join("\n")
}
