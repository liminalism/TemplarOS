//! Readback-only native device models. SSID identity is bytes; prompt secrets are never JSON.
use serde::{Deserialize, Serialize};
use templar_types::settings::{Capability, Revision};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PromptKind {
    Password,
    Pin,
    Passkey,
    Confirmation,
    Authorization,
    DisplayPin,
    DisplayPasskey,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DevicePrompt {
    pub token: String,
    pub subject: String,
    pub label: String,
    pub kind: PromptKind,
    pub deadline_unix_ms: u64,
    pub entered: u16,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NetworkDevice {
    pub path: String,
    pub interface: String,
    pub kind: u32,
    pub state: u32,
    pub reason: u32,
    pub firmware_missing: bool,
    pub managed: bool,
    pub active_connection: String,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AccessPoint {
    pub path: String,
    pub device: String,
    pub ssid: Vec<u8>,
    pub label: String,
    pub strength: u8,
    pub privacy: bool,
    pub wpa_flags: u32,
    pub rsn_flags: u32,
    pub personal_supported: bool,
}
impl AccessPoint {
    pub fn security(&self) -> &'static str {
        if self.rsn_flags & 0x400 != 0 {
            "WPA3 Personal"
        } else if (self.rsn_flags | self.wpa_flags) & 0x100 != 0 {
            "WPA Personal"
        } else if !self.privacy {
            "Open"
        } else {
            "Use the existing network provider for enterprise/legacy security"
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NetworkProfile {
    pub path: String,
    pub uuid: String,
    pub label: String,
    pub kind: String,
    pub autoconnect: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NetworkSnapshot {
    pub revision: Revision,
    pub capability: Capability,
    pub ui_owned: bool,
    pub state: u32,
    pub connectivity: u32,
    pub wireless_enabled: bool,
    pub wireless_hardware_enabled: bool,
    pub devices: Vec<NetworkDevice>,
    pub access_points: Vec<AccessPoint>,
    pub profiles: Vec<NetworkProfile>,
    pub prompts: Vec<DevicePrompt>,
    pub operation_error: Option<String>,
}
impl NetworkSnapshot {
    pub fn connectivity_label(&self) -> &'static str {
        match self.connectivity {
            1 => "Offline",
            2 => "Captive portal — login required",
            3 => "Limited connectivity",
            4 => "Internet connectivity verified by NetworkManager",
            _ => "Connectivity not checked",
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BluetoothAdapter {
    pub path: String,
    pub address: String,
    pub label: String,
    pub powered: bool,
    pub discovering: bool,
    /// This caller owns a native discovery request; global Discovering may belong to another client.
    pub discovery_owned: bool,
    pub pairable: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct BluetoothDevice {
    pub path: String,
    pub adapter: String,
    pub address: String,
    pub label: String,
    pub paired: bool,
    pub connected: bool,
    pub trusted: bool,
    pub blocked: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RadioBlock {
    pub name: String,
    pub software: bool,
    pub hardware: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BluetoothSnapshot {
    pub revision: Revision,
    pub capability: Capability,
    pub ui_owned: bool,
    pub adapters: Vec<BluetoothAdapter>,
    pub devices: Vec<BluetoothDevice>,
    pub radio_blocks: Vec<RadioBlock>,
    pub prompts: Vec<DevicePrompt>,
    pub operation_error: Option<String>,
    pub pairing: Option<String>,
}

pub fn ssid_label(ssid: &[u8]) -> String {
    if ssid.is_empty() {
        return "Hidden network".into();
    }
    let mut label = String::with_capacity(ssid.len());
    for character in String::from_utf8_lossy(ssid).chars() {
        if character.is_control() {
            label.extend(character.escape_default());
        } else {
            label.push(character);
        }
    }
    label
}

pub fn network_device_state(state: u32) -> &'static str {
    match state {
        10 => "Unmanaged",
        20 => "Unavailable",
        30 => "Disconnected",
        40 | 50 => "Activating",
        60 => "Credentials required",
        70 => "Obtaining addresses",
        80 => "Checking connection",
        90 => "Waiting for secondary connection",
        100 => "Connected",
        110 => "Disconnecting",
        120 => "Connection failed",
        _ => "Device state unknown",
    }
}

pub fn network_reason(reason: u32) -> &'static str {
    match reason {
        0 => "No reason provided",
        7 => "Secrets were required but not provided — check the native agent lease",
        10 => "Authentication/supplicant failed — check the password",
        9 => "Supplicant configuration failed",
        11 => "Authentication timed out — check password and signal",
        35 => "Firmware is missing",
        39 => "Disconnected by user or client",
        53 => "SSID was not found",
        3 => "Device is not managed by NetworkManager",
        _ => "NetworkManager device reason (numeric detail shown below)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ssid_display_does_not_change_lossless_identity_or_execute_controls() {
        let identity = vec![0xff, b'a', b'\n', 0];
        let label = ssid_label(&identity);
        assert!(label.contains("\\n"));
        assert!(!label.contains('\n'));
        assert_eq!(identity, [0xff, b'a', b'\n', 0]);
    }
}
