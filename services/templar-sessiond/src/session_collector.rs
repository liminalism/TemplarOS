//! Supported public Claude CLI inventory, invoked only at tending moments.
//! Fixed typed argv; no private-store parsing, shell interpolation, or recurring poll.
use std::{path::Path, process::Stdio, time::Duration};
use anyhow::{bail, Context, Result};
use tokio::{io::AsyncReadExt, process::Command};

pub async fn collect_claude_agents(project: &Path) -> Result<String> {
    if !project.is_absolute() { bail!("agent inventory requires an existing absolute project"); }
    let mut child = Command::new("claude")
        .args(["agents", "--json", "--all", "--cwd"]).arg(project)
        .current_dir(project).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null())
        .kill_on_drop(true).spawn().context("Claude public session inventory unavailable")?;
    let stdout = child.stdout.take().context("Claude inventory stdout unavailable")?;
    let operation = async {
        let read = async {
            let mut bytes = Vec::new();
            stdout.take(262_145).read_to_end(&mut bytes).await?;
            if bytes.len() > 262_144 { bail!("Claude inventory exceeds bounded output"); }
            Ok::<_, anyhow::Error>(bytes)
        };
        let wait = async { Ok::<_, anyhow::Error>(child.wait().await?) };
        let (bytes, status) = tokio::try_join!(read, wait)?;
        if !status.success() { bail!("Claude public session inventory returned an unsuccessful exit"); }
        String::from_utf8(bytes).context("Claude public session inventory is not UTF-8")
    };
    tokio::time::timeout(Duration::from_secs(5), operation).await
        .context("Claude public session inventory exceeded five-second deadline")?
}
