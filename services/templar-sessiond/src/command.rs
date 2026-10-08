//! Bounded, cancellable subprocesses for session adapters. No shell expansion.
use anyhow::{Context, Result, bail};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt};

async fn bounded_read(reader: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(limit as u64 + 1).read_to_end(&mut bytes).await?;
    if bytes.len() > limit { bail!("helper output exceeded {limit} bytes"); }
    Ok(bytes)
}

pub async fn run(program: &str, arguments: &[&str], stdout_limit: usize) -> Result<String> {
    let mut child = tokio::process::Command::new(program)
        .args(arguments).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .kill_on_drop(true).spawn().with_context(|| format!("starting {program}"))?;
    let stdout = child.stdout.take().context("helper stdout unavailable")?;
    let stderr = child.stderr.take().context("helper stderr unavailable")?;
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::try_join!(bounded_read(stdout, stdout_limit), bounded_read(stderr, 65536), async {
            child.wait().await.map_err(anyhow::Error::from)
        })
    }).await;
    let (stdout, stderr, status) = match result {
        Ok(Ok(output)) => output,
        failed => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return match failed {
                Ok(Err(error)) => Err(error).with_context(|| format!("running {program}")),
                Err(_) => anyhow::bail!("{program} exceeded its 3-second deadline"),
                Ok(Ok(_)) => unreachable!(),
            };
        }
    };
    if !status.success() {
        bail!("{program} failed ({status}): {}", String::from_utf8_lossy(&stderr).trim());
    }
    String::from_utf8(stdout).with_context(|| format!("{program} returned non-UTF-8 output"))
}
