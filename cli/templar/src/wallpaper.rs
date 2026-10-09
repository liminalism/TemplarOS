//! Native wallpaper controls: Settings1 owns configuration; Wallpaper1 owns runtime actions.
use anyhow::{Context, Result, bail};
use clap::Subcommand;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
use templar_client::{proxies::SettingsProxy, wallpaper::WallpaperClient};
use templar_types::wallpaper::{WallpaperConfig, WallpaperStatus};

#[derive(Debug, Subcommand)]
pub enum WallpaperCommand {
    /// Inspect desired configuration, pending work and actually presented output generations.
    Inspect,
    /// Choose the next source (works while paused).
    Next,
    /// Return to the previous successful source.
    Previous,
    /// Stop timed rotation, preserving the displayed image.
    Pause,
    /// Resume timed rotation.
    Resume,
    /// Reconcile/reload the current wallpaper.
    Refresh,
    /// Enable wallpaper through Settings1 (recorded in History).
    Enable,
    /// Disable wallpaper through Settings1 (recorded in History).
    Disable,
    /// Reset the enable gate to its Settings1 default, leaving policy unchanged.
    ResetEnabled,
    /// Lossless policy configuration; paths in JSON are Unix byte arrays.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the committed WallpaperConfig JSON, including raw source bytes and output overrides.
    Show,
    /// Replace policy from a JSON file, or stdin with '-'. Settings1 validates all fields.
    /// Fields: sources, recursive, order (random/sequential/shuffle), interval_seconds
    /// (null or 300..3600), sizing (fill/fit/stretch/center/actual/tile), ground [R,G,B,255],
    /// texture ({kind:native}, {kind:max_edge,pixels:N}, {kind:max_pixels,pixels:N}),
    /// worker_budget_bytes, transition_budget_bytes, quantize_colors (null or 1..256), outputs.
    Set { file: PathBuf },
    /// Reset policy to the Settings1 schema default (does not change the enabled key).
    Reset,
    /// Append a file/directory pathname, preserving non-UTF8 Unix bytes from the shell argument.
    AddSource { path: PathBuf },
    /// Append a pathname from even-length hexadecimal bytes; no lossy UTF8 conversion.
    AddSourceBytes { hex: String },
    /// Remove a zero-based source entry, as listed by config show.
    RemoveSource { index: usize },
}

pub fn decode_hex(hex: &str) -> Result<Vec<u8>> {
    if hex.len() % 2 != 0 || !hex.is_ascii() {
        bail!("path bytes require even-length ASCII hexadecimal");
    }
    hex.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair)?;
            u8::from_str_radix(text, 16).context("invalid hexadecimal path byte")
        })
        .collect()
}

pub fn path_text(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    match std::str::from_utf8(bytes) {
        Ok(path) => format!("{path:?} (bytes:{hex})"),
        Err(_) => format!("bytes:{hex} (non-UTF8 Unix path)"),
    }
}

pub fn render_status(status: &WallpaperStatus) -> String {
    let mut lines = vec![
        format!(
            "Desired: enabled={} paused={} pending={} revision={:?}",
            status.enabled, status.paused, status.pending, status.revision
        ),
        format!(
            "Source: {}",
            status
                .source
                .as_deref()
                .map_or_else(|| "none".into(), path_text)
        ),
        format!(
            "Policy: {:?}, {:?}, interval={:?}; texture={:?}; WuDP={:?}",
            status.config.order,
            status.config.sizing,
            status.config.interval_seconds,
            status.config.texture,
            status.config.quantize_colors
        ),
        format!(
            "Budgets: worker={} bytes; transition={} bytes",
            status.config.worker_budget_bytes, status.config.transition_budget_bytes
        ),
    ];
    match &status.applied {
        None => lines.push("Applied: no compositor observation (not proof of presentation)".into()),
        Some(applied) => {
            lines.push(format!(
                "Applied: active={} indexed_supported={} revision={:?}",
                applied.active(),
                applied.indexed_supported,
                applied.revision
            ));
            for output in &applied.outputs {
                lines.push(format!(
                    "  {}: accepted={:?} presented={:?} backing={} texture={} replicas={} error={}",
                    output.output.0,
                    output.accepted,
                    output.presented,
                    output.source_bytes,
                    output.texture_bytes,
                    output.context_replicas,
                    output.error.as_deref().unwrap_or("none")
                ));
            }
        }
    }
    lines.push(format!(
        "Error: {}",
        status.error.as_deref().unwrap_or("none")
    ));
    lines.join("\n")
}

async fn read_config(settings: &SettingsProxy<'_>) -> Result<WallpaperConfig> {
    let wire: serde_json::Value =
        serde_json::from_str(&settings.get("shell.wallpaper.config").await?)?;
    let json = wire
        .get("value")
        .and_then(serde_json::Value::as_str)
        .context("Settings1 wallpaper config is not a string value")?;
    serde_json::from_str(json).context("invalid committed wallpaper policy JSON")
}

async fn save_config(settings: &SettingsProxy<'_>, config: &WallpaperConfig) -> Result<()> {
    let wire = serde_json::json!({"type":"str", "value":serde_json::to_string(config)?});
    settings
        .set(
            "shell.wallpaper.config",
            &wire.to_string(),
            "templar-wallpaper-cli",
        )
        .await
        .context("Settings1.Set wallpaper config")
}

pub async fn run(command: WallpaperCommand, json: bool) -> Result<()> {
    let connection = crate::bus::session().await?;
    if let WallpaperCommand::Config { command } = command {
        let settings = SettingsProxy::new(&connection).await?;
        match command {
            ConfigCommand::Show => {}
            ConfigCommand::Reset => {
                settings
                    .reset("shell.wallpaper.config", "templar-wallpaper-cli")
                    .await?
            }
            ConfigCommand::Set { file } => {
                let text = if file.as_os_str() == "-" {
                    use std::io::Read as _;
                    let mut text = String::new();
                    std::io::stdin()
                        .read_to_string(&mut text)
                        .context("reading wallpaper JSON from stdin")?;
                    text
                } else {
                    std::fs::read_to_string(&file).context("reading wallpaper JSON file")?
                };
                let config: WallpaperConfig =
                    serde_json::from_str(&text).context("parsing WallpaperConfig JSON")?;
                save_config(&settings, &config).await?;
            }
            other => {
                let mut config = read_config(&settings).await?;
                match other {
                    ConfigCommand::AddSource { path } => {
                        config.sources.push(path.as_os_str().as_bytes().to_vec())
                    }
                    ConfigCommand::AddSourceBytes { hex } => config.sources.push(decode_hex(&hex)?),
                    ConfigCommand::RemoveSource { index } => {
                        if index >= config.sources.len() {
                            bail!(
                                "source index {index} is out of range ({} entries)",
                                config.sources.len()
                            );
                        }
                        config.sources.remove(index);
                    }
                    _ => unreachable!(),
                }
                save_config(&settings, &config).await?;
            }
        }
        // Read back the authoritative stored configuration, not the attempted write.
        println!(
            "{}",
            serde_json::to_string_pretty(&read_config(&settings).await?)?
        );
        return Ok(());
    }
    let client = WallpaperClient::new(&connection).await?;
    let status = match command {
        WallpaperCommand::Inspect => client.inspect().await?,
        WallpaperCommand::Next => client.next().await?,
        WallpaperCommand::Previous => client.previous().await?,
        WallpaperCommand::Pause => client.pause().await?,
        WallpaperCommand::Resume => client.resume().await?,
        WallpaperCommand::Refresh => client.refresh().await?,
        WallpaperCommand::Enable | WallpaperCommand::Disable => {
            let enabled = matches!(command, WallpaperCommand::Enable);
            let settings = SettingsProxy::new(&connection).await?;
            settings
                .set(
                    "shell.wallpaper.enabled",
                    &serde_json::json!({"type":"bool", "value":enabled}).to_string(),
                    "templar-wallpaper-cli",
                )
                .await?;
            client.inspect().await?
        }
        WallpaperCommand::ResetEnabled => {
            let settings = SettingsProxy::new(&connection).await?;
            settings
                .reset("shell.wallpaper.enabled", "templar-wallpaper-cli")
                .await?;
            client.inspect().await?
        }
        WallpaperCommand::Config { .. } => unreachable!(),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        println!("{}", render_status(&status));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_paths_are_lossless_and_bad_hex_is_refused() {
        assert_eq!(decode_hex("2fff80").unwrap(), vec![b'/', 255, 128]);
        assert!(path_text(&[b'/', 255]).contains("bytes:2fff"));
        assert!(decode_hex("f").is_err());
        assert!(decode_hex("zz").is_err());
    }
    #[test]
    fn clap_keeps_non_utf8_source_arguments() {
        use clap::Parser;
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;
        #[derive(Parser)]
        struct Args {
            #[command(subcommand)]
            command: WallpaperCommand,
        }
        let args = Args::try_parse_from([
            OsString::from("wallpaper"),
            OsString::from("config"),
            OsString::from("add-source"),
            OsString::from_vec(vec![b'/', 255]),
        ])
        .unwrap();
        let WallpaperCommand::Config {
            command: ConfigCommand::AddSource { path },
        } = args.command
        else {
            panic!("wrong command");
        };
        assert_eq!(path.as_os_str().as_bytes(), &[b'/', 255]);
    }
}
