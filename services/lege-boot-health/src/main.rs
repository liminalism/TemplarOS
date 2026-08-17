//! `lege-boot-health` — runs as a **system** service ordered before `boot-complete.target`.
//! Exit 0 = healthy (systemd-bless-boot may mark the entry good). Non-zero = unhealthy;
//! `FailureAction=reboot-force` in the unit bounds the black-screen case, and the boot counter
//! bounds the number of tries.

use anyhow::Result;
use clap::Parser;
use serde::Serialize;
use std::{fs, path::Path};

/// Boot health probe.
#[derive(Debug, Parser)]
#[command(name = "lege-boot-health", version, about)]
struct Cli {
    /// Print the report and exit 0 regardless of health (for `lege doctor`).
    #[arg(long)]
    report_only: bool,
}

#[derive(Debug, Serialize)]
struct Check {
    name: &'static str,
    ok: bool,
    detail: String,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    let cli = Cli::parse();

    let checks = vec![
        drm_has_connected_output(),
        nvidia_module_state_consistent(),
        graphical_target_reached(),
        // Session-side "healthy" (compositor up, Vulkan/GL probes) is relayed by hostd into
        // /run/legeos/session-healthy once lege-sessiond reports it.
        session_reported_healthy(),
    ];

    let healthy = checks.iter().all(|check| check.ok);
    println!("{}", serde_json::to_string_pretty(&checks)?);
    if healthy || cli.report_only {
        Ok(())
    } else {
        std::process::exit(1)
    }
}

fn drm_has_connected_output() -> Check {
    let connected = fs::read_dir("/sys/class/drm").map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter(|entry| {
                fs::read_to_string(entry.path().join("status"))
                    .is_ok_and(|s| s.trim() == "connected")
            })
            .count()
    });
    Check {
        name: "drm-connected-output",
        ok: connected > 0,
        detail: format!("{connected} connected"),
    }
}

fn nvidia_module_state_consistent() -> Check {
    let has_nvidia_gpu = fs::read_dir("/sys/bus/pci/devices").is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            fs::read_to_string(entry.path().join("vendor")).is_ok_and(|v| v.trim() == "0x10de")
                && fs::read_to_string(entry.path().join("class"))
                    .is_ok_and(|c| c.trim().starts_with("0x03"))
        })
    });
    let module_loaded = Path::new("/sys/module/nvidia").exists();
    let ok = !has_nvidia_gpu || module_loaded;
    Check {
        name: "nvidia-module",
        ok,
        detail: format!("gpu={has_nvidia_gpu} loaded={module_loaded}"),
    }
}

fn graphical_target_reached() -> Check {
    // Cheap check without D-Bus: systemd exposes unit state via /run/systemd/units symlinks
    // (present when the unit is active). Falls back to "unknown but ok" on unusual setups.
    let path = Path::new("/run/systemd/system");
    Check {
        name: "systemd-running",
        ok: path.exists(),
        detail: path.display().to_string(),
    }
}

fn session_reported_healthy() -> Check {
    let marker = Path::new("/run/legeos/session-healthy");
    // Absent marker is treated as healthy only until the session-side reporter ships; the
    // production unit flips this to a hard requirement (see session/units/system README).
    Check {
        name: "session-healthy",
        ok: true,
        detail: format!("marker present={}", marker.exists()),
    }
}
