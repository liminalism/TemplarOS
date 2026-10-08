//! The compositor owns transactions/deadlines. Every UI operation is followed by fresh readback.
use anyhow::Result;
use templar_client::proxies::DisplaysProxy;
pub use templar_display_ui::{DisplaysCommand, DisplaysState, DisplaysView};

#[derive(Default)]
pub struct DisplaysController { connection: Option<zbus::Connection> }
impl DisplaysController {
    pub async fn run(&mut self, command: DisplaysCommand) -> DisplaysState {
        if self.connection.is_none() {
            match zbus::Connection::session().await { Ok(connection) => self.connection = Some(connection), Err(error) => return DisplaysState { observation: None, message: Some(format!("Native display broker unavailable: {error}")) } }
        }
        let connection = self.connection.as_ref().expect("connected above");
        let operation: Result<()> = async {
            let proxy = DisplaysProxy::new(connection).await?;
            match command {
                DisplaysCommand::Refresh => {},
                DisplaysCommand::Validate { expected, profile } => { proxy.validate(&serde_json::to_string(&expected)?, &serde_json::to_string(&profile)?).await?; },
                DisplaysCommand::Preview { expected, profile } => { proxy.preview(&serde_json::to_string(&expected)?, &serde_json::to_string(&profile)?).await?; },
                DisplaysCommand::Confirm { expected, token } => { proxy.confirm(&serde_json::to_string(&expected)?, &token.to_string()).await?; },
                DisplaysCommand::Revert { token } => { proxy.revert(&token.to_string()).await?; },
                DisplaysCommand::Identify { expected, outputs, calibration } => { proxy.identify(&serde_json::to_string(&expected)?, &serde_json::to_string(&outputs)?, calibration).await?; },
            }
            Ok(())
        }.await;
        let readback: Result<_> = async { Ok(serde_json::from_str(&DisplaysProxy::new(connection).await?.observe().await?)?) }.await;
        match readback {
            Ok(observation) => DisplaysState { observation: Some(observation), message: operation.err().map(|error| format!("Could not change displays: {error:#}; the state below is fresh actual readback")) },
            Err(error) => DisplaysState { observation: None, message: Some(match operation { Ok(()) => format!("Native display state unavailable: {error:#}"), Err(write) => format!("Display operation failed: {write:#}; readback unavailable: {error:#}") }) },
        }
    }
}
pub async fn print() -> Result<()> {
    let state = DisplaysController::default().run(DisplaysCommand::Refresh).await;
    if let Some(message) = state.message { println!("{message}"); }
    if let Some(observation) = state.observation { println!("{}", serde_json::to_string_pretty(&observation)?); }
    Ok(())
}
