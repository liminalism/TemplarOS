//! The live preflight: can this machine actually run a TemplarOS session, asked *before* anything is
//! written to a disk.
//!
//! The installation medium's whole job is not to strand its owner. Every check here answers one
//! question a person would otherwise only discover after an install — is there a display device a
//! compositor can drive, is there a network interface with a driver behind it, did the radio's
//! firmware load, is there a sound card, are there input devices — and each answers it from
//! `/sys`, `/proc` and `/dev` under [`Context::root`], so the whole section is testable against a
//! fixture tree and never probes the machine running the tests.
//!
//! Two rules hold the honesty line. Nothing here is promoted to a pass: a question this machine
//! cannot answer is `Skipped`, and one that needs a tool on the live system while `--root` points
//! elsewhere is `NotExecuted`. And only a genuine hardware gap carries `Severity::Error`, because
//! `templar doctor --preflight`'s exit code is what `templaros-install` refuses on.

use super::{Context, Finding, Outcome, Severity};
use std::{fs, path::Path, time::Duration};

/// Interfaces that are never evidence of hardware networking.
const VIRTUAL_INTERFACES: &[&str] = &["lo", "docker", "veth", "virbr", "br-", "tun", "tap", "wg"];

/// How long `nmcli` gets before the preflight decides the answer is not coming.
const TOOL_TIMEOUT: Duration = Duration::from_secs(5);

pub fn findings(context: &Context) -> Vec<Finding> {
    let root = context.root.as_path();
    vec![
        drm(root),
        render_node(root),
        network(root),
        network_manager(context),
        wifi_radio(root),
        audio(root),
        input(root),
    ]
}

fn make(
    id: &'static str,
    severity: Severity,
    outcome: Outcome,
    summary: impl Into<String>,
    detail: impl Into<String>,
) -> Finding {
    Finding {
        id,
        severity,
        outcome,
        summary: summary.into(),
        detail: detail.into(),
    }
}

/// Directory entries of `root/relative`, sorted, or an empty list if it cannot be read.
fn entries(root: &Path, relative: &str) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(root.join(relative.trim_start_matches('/')))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The last component of the driver symlink for a device directory — the module actually bound.
fn driver_of(path: &Path) -> Option<String> {
    let target = fs::read_link(path.join("device/driver")).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

/// A DRM device with a driver bound, and whether anything is connected to it.
///
/// This is the check that separates "the compositor will start" from "black screen": a card node
/// with no driver means the kernel has no modesetting driver for this GPU, which is exactly what
/// the safe-graphics boot entry exists to work around.
fn drm(root: &Path) -> Finding {
    let class = root.join("sys/class/drm");
    let mut bound = Vec::new();
    for name in entries(root, "sys/class/drm") {
        if !name.starts_with("card") || name.contains('-') {
            continue;
        }
        if let Some(driver) = driver_of(&class.join(&name)) {
            let connected: Vec<String> = entries(root, "sys/class/drm")
                .iter()
                .filter(|c| c.starts_with(&format!("{name}-")))
                .filter(|c| {
                    fs::read_to_string(class.join(c).join("status"))
                        .is_ok_and(|s| s.trim() == "connected")
                })
                .map(|c| c[name.len() + 1..].to_owned())
                .collect();
            bound.push(format!(
                "{name} ({driver}, connected: {})",
                if connected.is_empty() {
                    "none".to_owned()
                } else {
                    connected.join("+")
                }
            ));
        }
    }
    if bound.is_empty() {
        make(
            "preflight-drm",
            Severity::Error,
            Outcome::Fail,
            "no DRM device with a driver bound",
            "nothing under /sys/class/drm has a driver: the session would start on a blank screen. \
             Boot the medium's safe-graphics entry and report the GPU.",
        )
    } else {
        make(
            "preflight-drm",
            Severity::Error,
            Outcome::Pass,
            format!("{} DRM device(s) with a driver", bound.len()),
            bound.join(", "),
        )
    }
}

/// A render node, which is what clients and the compositor's GL path open.
fn render_node(root: &Path) -> Finding {
    let nodes: Vec<String> = entries(root, "dev/dri")
        .into_iter()
        .filter(|n| n.starts_with("renderD"))
        .collect();
    if nodes.is_empty() {
        make(
            "preflight-render-node",
            Severity::Warning,
            Outcome::Fail,
            "no render node under /dev/dri",
            "hardware acceleration is unavailable; the session may fall back to software rendering",
        )
    } else {
        make(
            "preflight-render-node",
            Severity::Warning,
            Outcome::Pass,
            format!("{} render node(s)", nodes.len()),
            nodes.join(", "),
        )
    }
}

/// Physical network interfaces are those that are not the loopback or a software bridge.
fn physical_interfaces(root: &Path) -> Vec<(String, String, bool)> {
    entries(root, "sys/class/net")
        .into_iter()
        .filter(|name| {
            !VIRTUAL_INTERFACES
                .iter()
                .any(|prefix| name.as_str() == *prefix || name.starts_with(prefix))
        })
        .filter_map(|name| {
            let path = root.join("sys/class/net").join(&name);
            let wireless = path.join("wireless").is_dir();
            driver_of(&path).map(|driver| (name, driver, wireless))
        })
        .collect()
}

/// At least one interface with a driver. A radio counts: the reference laptop has no wired port
/// at all, so demanding an ethernet device would fail the very machine this medium is for.
fn network(root: &Path) -> Finding {
    let interfaces = physical_interfaces(root);
    if interfaces.is_empty() {
        make(
            "preflight-network",
            Severity::Error,
            Outcome::Fail,
            "no network interface has a driver",
            "no wired or wireless device under /sys/class/net is bound to a driver: this medium \
             cannot reach a network, and an installed system would not either",
        )
    } else {
        make(
            "preflight-network",
            Severity::Error,
            Outcome::Pass,
            format!("{} network interface(s) with a driver", interfaces.len()),
            interfaces
                .iter()
                .map(|(name, driver, wireless)| {
                    format!(
                        "{name} ({driver}{})",
                        if *wireless { ", radio" } else { "" }
                    )
                })
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

/// NetworkManager is answering. This one runs a tool, so it is `NotExecuted` off `/`.
fn network_manager(context: &Context) -> Finding {
    if context.root != Path::new("/") {
        return make(
            "preflight-network-manager",
            Severity::Warning,
            Outcome::NotExecuted,
            "nmcli would describe this host, not the inspected root",
            format!("--root {}", context.root.display()),
        );
    }
    let answer = templar_hwdetect::probes::run_with_timeout(
        "nmcli",
        &["-t", "-f", "RUNNING,STATE", "general"],
        TOOL_TIMEOUT,
    );
    match answer {
        Ok(output) => {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            if text.starts_with("running") {
                make(
                    "preflight-network-manager",
                    Severity::Warning,
                    Outcome::Pass,
                    "NetworkManager is running",
                    text,
                )
            } else {
                make(
                    "preflight-network-manager",
                    Severity::Warning,
                    Outcome::Fail,
                    "NetworkManager is not running",
                    if text.is_empty() {
                        String::from_utf8_lossy(&output.stderr).trim().to_owned()
                    } else {
                        text
                    },
                )
            }
        }
        Err(error) => make(
            "preflight-network-manager",
            Severity::Warning,
            Outcome::Skipped,
            "nmcli did not answer",
            error.to_string(),
        ),
    }
}

/// A registered radio. `/sys/class/ieee80211/phy*` appears only once the driver has finished
/// bringing the device up, which for every firmware-loading Wi-Fi driver means its firmware
/// loaded — so this is the cheapest honest answer to "did the Wi-Fi firmware make it onto the
/// medium", and it needs no kernel log.
fn wifi_radio(root: &Path) -> Finding {
    let radios = entries(root, "sys/class/ieee80211");
    let wireless: Vec<String> = physical_interfaces(root)
        .into_iter()
        .filter(|(_, _, wireless)| *wireless)
        .map(|(name, driver, _)| format!("{name} ({driver})"))
        .collect();
    if wireless.is_empty() && radios.is_empty() {
        return make(
            "preflight-wifi",
            Severity::Warning,
            Outcome::Skipped,
            "no Wi-Fi device on this machine",
            "nothing under /sys/class/ieee80211",
        );
    }
    if radios.is_empty() {
        return make(
            "preflight-wifi",
            Severity::Warning,
            Outcome::Fail,
            "a Wi-Fi device exists but no radio registered",
            format!(
                "{}: the driver bound but /sys/class/ieee80211 is empty, which is what a missing \
                 firmware file looks like",
                wireless.join(", ")
            ),
        );
    }
    make(
        "preflight-wifi",
        Severity::Warning,
        Outcome::Pass,
        format!("{} radio(s) registered", radios.len()),
        format!("{} [{}]", radios.join(", "), wireless.join(", ")),
    )
}

/// A sound card. `/proc/asound/cards` is the one file that answers for every ALSA path.
fn audio(root: &Path) -> Finding {
    let Ok(text) = fs::read_to_string(root.join("proc/asound/cards")) else {
        return make(
            "preflight-audio",
            Severity::Warning,
            Outcome::Skipped,
            "no /proc/asound: ALSA is not loaded here",
            "",
        );
    };
    let cards: Vec<&str> = text
        .lines()
        .filter(|line| line.contains(": "))
        .map(str::trim)
        .collect();
    if cards.is_empty() {
        make(
            "preflight-audio",
            Severity::Warning,
            Outcome::Fail,
            "no sound card",
            "/proc/asound/cards is empty: the codec driver or its firmware did not load",
        )
    } else {
        make(
            "preflight-audio",
            Severity::Warning,
            Outcome::Pass,
            format!("{} sound card(s)", cards.len()),
            cards.join("; "),
        )
    }
}

/// Input devices. A live medium with no keyboard or pointer cannot be used to install anything.
fn input(root: &Path) -> Finding {
    let names: Vec<String> = entries(root, "sys/class/input")
        .into_iter()
        .filter(|name| name.starts_with("event"))
        .filter_map(|name| {
            fs::read_to_string(root.join("sys/class/input").join(&name).join("device/name")).ok()
        })
        .map(|name| name.trim().to_owned())
        .collect();
    if names.is_empty() {
        make(
            "preflight-input",
            Severity::Error,
            Outcome::Fail,
            "no input devices",
            "nothing under /sys/class/input: there is no way to drive this session",
        )
    } else {
        make(
            "preflight-input",
            Severity::Warning,
            Outcome::Pass,
            format!("{} input device(s)", names.len()),
            names.join(", "),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A fixture root shaped like the reference laptop: Intel iGPU with a panel, an NVIDIA card
    /// with nothing attached, one Realtek radio, a registered phy, two HDA cards, a keyboard.
    fn laptop() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let drm = root.join("sys/class/drm");
        for (card, driver) in [("card1", "nvidia"), ("card2", "i915")] {
            fs::create_dir_all(drm.join(card).join("device")).unwrap();
            fs::create_dir_all(root.join("sys/bus/pci/drivers").join(driver)).unwrap();
            std::os::unix::fs::symlink(
                root.join("sys/bus/pci/drivers").join(driver),
                drm.join(card).join("device/driver"),
            )
            .unwrap();
        }
        fs::create_dir_all(drm.join("card2-eDP-2")).unwrap();
        fs::write(drm.join("card2-eDP-2/status"), "connected\n").unwrap();
        fs::create_dir_all(drm.join("card1-HDMI-A-1")).unwrap();
        fs::write(drm.join("card1-HDMI-A-1/status"), "disconnected\n").unwrap();
        fs::create_dir_all(root.join("dev/dri")).unwrap();
        fs::write(root.join("dev/dri/renderD128"), "").unwrap();

        let net = root.join("sys/class/net/wlp55s0");
        fs::create_dir_all(net.join("wireless")).unwrap();
        fs::create_dir_all(net.join("device")).unwrap();
        fs::create_dir_all(root.join("sys/bus/pci/drivers/rtw89_8852ce")).unwrap();
        std::os::unix::fs::symlink(
            root.join("sys/bus/pci/drivers/rtw89_8852ce"),
            net.join("device/driver"),
        )
        .unwrap();
        fs::create_dir_all(root.join("sys/class/net/lo")).unwrap();
        fs::create_dir_all(root.join("sys/class/ieee80211/phy0")).unwrap();

        fs::create_dir_all(root.join("proc/asound")).unwrap();
        fs::write(
            root.join("proc/asound/cards"),
            " 0 [PCH            ]: HDA-Intel - HDA Intel PCH\n                      HDA Intel PCH at 0x1\n",
        )
        .unwrap();
        let event = root.join("sys/class/input/event0/device");
        fs::create_dir_all(&event).unwrap();
        fs::write(event.join("name"), "AT Translated Set 2 keyboard\n").unwrap();
        dir
    }

    fn context(root: &Path) -> Context {
        Context {
            root: root.to_path_buf(),
            hardware: true,
        }
    }

    fn outcome(findings: &[Finding], id: &str) -> Outcome {
        findings.iter().find(|f| f.id == id).unwrap().outcome
    }

    #[test]
    fn a_working_laptop_passes_every_hardware_question() {
        let dir = laptop();
        let findings = findings(&context(dir.path()));
        for id in [
            "preflight-drm",
            "preflight-render-node",
            "preflight-network",
            "preflight-wifi",
            "preflight-audio",
            "preflight-input",
        ] {
            assert_eq!(outcome(&findings, id), Outcome::Pass, "{id}");
        }
        // The tool probe never runs against a fixture root.
        assert_eq!(
            outcome(&findings, "preflight-network-manager"),
            Outcome::NotExecuted
        );
        assert_eq!(super::super::exit_code(&findings), 0);
        let drm = findings.iter().find(|f| f.id == "preflight-drm").unwrap();
        assert!(
            drm.detail.contains("card2 (i915, connected: eDP-2)"),
            "{drm:?}"
        );
    }

    #[test]
    fn a_machine_with_no_driver_bound_fails_and_exits_non_zero() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A GPU and a NIC are present but nothing is bound to either, and there is no input.
        fs::create_dir_all(root.join("sys/class/drm/card0/device")).unwrap();
        fs::create_dir_all(root.join("sys/class/net/enp0s1/device")).unwrap();
        let findings = findings(&context(root));
        assert_eq!(outcome(&findings, "preflight-drm"), Outcome::Fail);
        assert_eq!(outcome(&findings, "preflight-network"), Outcome::Fail);
        assert_eq!(outcome(&findings, "preflight-input"), Outcome::Fail);
        assert_eq!(super::super::exit_code(&findings), 1);
    }

    #[test]
    fn a_radio_whose_firmware_did_not_load_is_a_failure_not_a_skip() {
        let dir = laptop();
        fs::remove_dir_all(dir.path().join("sys/class/ieee80211/phy0")).unwrap();
        let findings = findings(&context(dir.path()));
        assert_eq!(outcome(&findings, "preflight-wifi"), Outcome::Fail);
        // A radio is a warning, so the medium is still usable: exit 2, not 1.
        assert_eq!(super::super::exit_code(&findings), 2);
    }

    #[test]
    fn a_machine_with_no_wifi_at_all_skips_rather_than_failing() {
        let dir = laptop();
        fs::remove_dir_all(dir.path().join("sys/class/net/wlp55s0")).unwrap();
        fs::remove_dir_all(dir.path().join("sys/class/ieee80211/phy0")).unwrap();
        let findings = findings(&context(dir.path()));
        assert_eq!(outcome(&findings, "preflight-wifi"), Outcome::Skipped);
        assert_eq!(outcome(&findings, "preflight-network"), Outcome::Fail);
    }
}
