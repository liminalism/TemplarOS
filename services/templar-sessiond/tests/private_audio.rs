//! Opt-in real PipeWire/WirePlumber lifecycle, confined by the native-audio-proof harness.
#![allow(dead_code)]

#[path = "../src/apis/audio.rs"]
mod audio;
#[path = "../src/command.rs"]
mod command;
mod apis {
    pub(crate) fn failed(error: impl std::fmt::Display) -> zbus::fdo::Error {
        zbus::fdo::Error::Failed(error.to_string())
    }
}

use anyhow::{Context, Result, ensure};
use audio::{AudioApi, AudioControl, WpctlAudio};
use futures_util::StreamExt;
use std::{path::Path, process::Stdio, sync::Arc, time::Duration};
use templar_client::{proxies::session::AudioProxy, status::{AudioInventory, AudioState}};
use templar_state::Store;

async fn native(program: &str, args: &[&str]) -> Result<String> {
    let result = command::run(program, args, 16 * 1024 * 1024).await;
    let record = serde_json::json!({"program": program, "arguments": args});
    use std::io::Write as _;
    writeln!(std::fs::OpenOptions::new().create(true).append(true).open("/proof/native-commands.jsonl")?, "{record}")?;
    println!("NATIVE_COMMAND {program} {:?} ({} argument bytes) -> {}", &args[..args.len().min(2)], args.iter().map(|arg| arg.len()).sum::<usize>(), match &result {
        Ok(stdout) => format!("success {} bytes", stdout.len()), Err(error) => format!("{error:#}"),
    });
    result
}

async fn inventory(proxy: &AudioProxy<'_>) -> Result<AudioInventory> {
    Ok(serde_json::from_str(&proxy.inventory().await?)?)
}

async fn state(proxy: &AudioProxy<'_>) -> Result<AudioState> {
    Ok(serde_json::from_str(&proxy.get().await?)?)
}

async fn create(name: &str, direction: &str, description: &str) -> Result<()> {
    native("pw-cli", &["create-node", "adapter", &format!(
        "{{ factory.name = support.null-audio-sink node.name = {name} node.description = \"{description}\" media.class = Audio/{direction} object.linger = true audio.position = [ FL FR ] }}",
    )]).await?;
    Ok(())
}

async fn signal<S: futures_util::Stream + Unpin>(signals: &mut S) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(8), signals.next()).await?
        .context("audio signal stream ended")?;
    Ok(())
}

fn owned_pid(key: &str) -> Result<String> {
    let pid = std::env::var(key)?;
    ensure!(pid.parse::<u32>()? > 1, "invalid owned child PID");
    Ok(pid)
}

async fn kill(pid: &str, signal: &str) -> Result<()> {
    native("/usr/bin/kill", &[signal, pid]).await?;
    Ok(())
}

async fn drain<S: futures_util::Stream + Unpin>(signals: &mut S) {
    while matches!(tokio::time::timeout(Duration::from_millis(150), signals.next()).await, Ok(Some(_))) {}
}

#[tokio::test]
#[ignore = "requires the owned native-audio-proof bwrap runtime; never run against host audio"]
async fn native_audio_lifecycle() -> Result<()> {
    ensure!(std::env::var("LEGEOS_OWNED_AUDIO_PROOF").as_deref() == Ok("/proof"), "missing owned-runtime authorization");
    ensure!(std::env::var("XDG_RUNTIME_DIR")? == "/proof/run");
    ensure!(std::env::var("PIPEWIRE_RUNTIME_DIR")? == "/proof/run");
    ensure!(std::env::var("DBUS_SESSION_BUS_ADDRESS")? == "unix:path=/proof/run/session-bus");
    ensure!(Path::new("/sys").read_dir()?.next().is_none(), "host sysfs exposed");
    ensure!(!Path::new("/dev/snd").exists(), "host audio devices exposed");
    let pw_pid = owned_pid("OWNED_PIPEWIRE_PID")?;
    let wp_pid = owned_pid("OWNED_WIREPLUMBER_PID")?;
    let store = Store::open(Path::new("/proof/state/audio.db"))?;
    let connection = zbus::connection::Builder::session()?
        .name("org.templaros.Session")?
        .serve_at(templar_types::dbus::session::AUDIO_PATH, AudioApi::new(Arc::new(WpctlAudio), store.clone()))?
        .build().await?;
    audio::spawn_watch(connection.clone());
    let client = zbus::Connection::session().await?;
    let proxy = AudioProxy::new(&client).await?;
    let mut signals = proxy.receive_changed().await?;
    let initial = inventory(&proxy).await?;
    println!("INITIAL_INVENTORY {}", serde_json::to_string(&initial)?);
    ensure!(initial.devices.len() == 4, "native nodes missing from production inventory");
    proxy.select_device("output", "owned.headset").await?;
    proxy.select_device("input", "owned.input2").await?;
    let selected = inventory(&proxy).await?;
    ensure!(selected.default_output.as_deref() == Some("owned.headset"));
    ensure!(selected.default_input.as_deref() == Some("owned.input2"));
    ensure!(selected.preferred_output == selected.default_output);
    ensure!(selected.preferred_input == selected.default_input);
    println!("SELECTED_INVENTORY {}", serde_json::to_string(&selected)?);
    proxy.set_volume(37).await?;
    proxy.set_muted(true).await?;
    ensure!(state(&proxy).await?.volume_percent == 37 && state(&proxy).await?.muted);
    drain(&mut signals).await;
    native("wpctl", &["set-volume", "@DEFAULT_AUDIO_SINK@", "61%"]).await?;
    signal(&mut signals).await?;
    ensure!(state(&proxy).await?.volume_percent == 61);
    drain(&mut signals).await;
    native("wpctl", &["set-mute", "@DEFAULT_AUDIO_SINK@", "0"]).await?;
    signal(&mut signals).await?;
    ensure!(!state(&proxy).await?.muted);
    let speakers = selected.devices.iter().find(|d| d.name == "owned.speakers").unwrap();
    drain(&mut signals).await;
    native("wpctl", &["set-default", &speakers.id.to_string()]).await?;
    signal(&mut signals).await?;
    let external = inventory(&proxy).await?;
    ensure!(external.default_output.as_deref() == Some("owned.speakers"));
    ensure!(external.preferred_output.as_deref() == Some("owned.headset"));
    ensure!(proxy.select_device("output", "owned.input2").await.is_err());
    ensure!(proxy.select_device("output", "owned.missing").await.is_err());
    ensure!(inventory(&proxy).await?.preferred_output == selected.preferred_output);
    proxy.select_device("output", "owned.headset").await?;
    let headset = selected.devices.iter().find(|d| d.name == "owned.headset").unwrap();
    drain(&mut signals).await;
    native("pw-cli", &["destroy", &headset.id.to_string()]).await?;
    signal(&mut signals).await?;
    let removed = inventory(&proxy).await?;
    ensure!(!removed.devices.iter().any(|d| d.name == "owned.headset"));
    ensure!(removed.default_output.as_deref() == Some("owned.speakers"));
    ensure!(removed.preferred_output == selected.preferred_output);
    ensure!(proxy.select_device("output", "owned.headset").await.is_err());
    drain(&mut signals).await;
    create("owned.headset", "Sink", "Recreated owned virtual headset").await?;
    signal(&mut signals).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let reappeared = inventory(&proxy).await?;
    let fresh = reappeared.devices.iter().find(|d| d.name == "owned.headset").unwrap();
    ensure!(fresh.serial != headset.serial);
    ensure!(reappeared.default_output.as_deref() == Some("owned.headset"));
    println!("REAPPEARED_INVENTORY {}", serde_json::to_string(&reappeared)?);
    let durable = Store::open(Path::new("/proof/state/audio.db"))?;
    let saved: String = durable.lock().query_row("SELECT value_json FROM settings_values WHERE key='_internal.audio.preferred-output'", [], |row| row.get(0))?;
    ensure!(serde_json::from_str::<String>(&saved)? == "owned.headset");
    println!("DURABLE_PREFERENCE_REOPENED {saved}");

    // Freeze the real server: the production helper must time out, not hold the bus forever.
    kill(&pw_pid, "-STOP").await?;
    let started = tokio::time::Instant::now();
    let stalled = state(&proxy).await?;
    let elapsed = started.elapsed();
    kill(&pw_pid, "-CONT").await?;
    ensure!(!stalled.present && stalled.unavailable_reason.as_deref().is_some_and(|r| r.contains("3-second deadline")));
    ensure!(elapsed < Duration::from_secs(5));
    println!("NATIVE_SERVER_STALL_GET {elapsed:?} {}", serde_json::to_string(&stalled)?);
    tokio::time::sleep(Duration::from_millis(200)).await;
    ensure!(state(&proxy).await?.present);

    // Without policy applying the configured default, a failed selection cannot become durable.
    kill(&wp_pid, "-STOP").await?;
    let started = tokio::time::Instant::now();
    let rejected = proxy.select_device("output", "owned.speakers").await;
    let elapsed = started.elapsed();
    kill(&wp_pid, "-CONT").await?;
    ensure!(rejected.is_err());
    ensure!(inventory(&proxy).await?.preferred_output.as_deref() == Some("owned.headset"));
    println!("NATIVE_POLICY_STALL_SELECTION {elapsed:?} {rejected:?}");
    proxy.select_device("output", "owned.headset").await?;

    // Actual native output, not a fake helper, exceeds both production helper bounds.
    let description = "x".repeat(70_000);
    create("owned.large", "Sink", &description).await?;
    let large = inventory(&proxy).await?.devices.into_iter().find(|d| d.name == "owned.large").unwrap();
    let inspected = WpctlAudio.wpctl(&["inspect", &large.id.to_string()]).await;
    ensure!(inspected.as_ref().is_err_and(|e| format!("{e:#}").contains("65536 bytes")));
    println!("REAL_WPCTL_OUTPUT_BOUND {inspected:?}");
    // Native metadata exercises the dump bound without making hundreds of active audio nodes.
    let payload = "y".repeat(90_000);
    for index in 0..100 {
        native("pw-metadata", &["--name", "default", "0", &format!("owned.bulk{index}"), &payload, "Spa:String"]).await?;
    }
    let oversized = WpctlAudio.inventory().await;
    ensure!(oversized.as_ref().is_err_and(|e| format!("{e:#}").contains("8388608 bytes")));
    println!("REAL_PW_DUMP_OUTPUT_BOUND {oversized:?}");
    let raw = native("pw-dump", &[]).await?;
    ensure!(raw.len() > 8 * 1024 * 1024);
    println!("REAL_OVERSIZED_DUMP_BYTES {}", raw.len());
    for index in 0..100 {
        native("pw-metadata", &["--name", "default", "--delete", "0", &format!("owned.bulk{index}")]).await?;
    }
    native("pw-cli", &["destroy", &large.id.to_string()]).await?;
    println!("REMOVED_REAL_BULK_METADATA 100");
    tokio::time::sleep(Duration::from_secs(3)).await;
    ensure!(inventory(&proxy).await?.devices.len() == 4);
    drain(&mut signals).await;
    kill(&pw_pid, "-TERM").await?;
    signal(&mut signals).await?;
    let lost = state(&proxy).await?;
    ensure!(!lost.present && lost.unavailable_reason.is_some());
    ensure!(proxy.inventory().await.is_err());
    println!("NATIVE_SERVER_LOSS_CHANGED {}", serde_json::to_string(&lost)?);
    ensure!(tokio::time::timeout(Duration::from_secs(3), signals.next()).await.is_err(), "duplicate disconnect transition");

    // Restart genuine native daemons in the same owned socket directory, then recreate routes.
    let mut pw = tokio::process::Command::new("pipewire").stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn()?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut wp = tokio::process::Command::new("wireplumber").arg("--profile=policy").stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn()?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    create("owned.speakers", "Sink", "Restarted owned speakers").await?;
    create("owned.headset", "Sink", "Restarted owned headset").await?;
    create("owned.input2", "Source", "Restarted owned microphone").await?;
    signal(&mut signals).await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let recovered = inventory(&proxy).await?;
    ensure!(recovered.default_output.as_deref() == Some("owned.headset"));
    ensure!(recovered.default_input.as_deref() == Some("owned.input2"));
    ensure!(state(&proxy).await?.present);
    println!("NATIVE_RESTART_RECOVERED {}", serde_json::to_string(&recovered)?);
    wp.kill().await?;
    wp.wait().await?;
    pw.kill().await?;
    pw.wait().await?;
    println!("OWNED_RESTART_CHILDREN_REAPED");
    Ok(())
}
