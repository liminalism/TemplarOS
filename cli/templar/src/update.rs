//! `templar update` — staged kernel/driver transactions, boot deployments, the driver authority and
//! the storage reserve (`docs/architecture/updates-drivers.md`, ADR-0004).
//!
//! These live on the **system** bus (`org.templaros.Host`), unlike everything else this CLI talks
//! to, and every mutating call is polkit-gated. Reads (`Current`, `History`, `List`, `Plan`,
//! `Violations`, `Usage`, `TrimOrder`) are not, so the common case — "what is my machine about to
//! boot, and why" — works without authenticating.
//!
//! Every D-Bus call goes through a client trait so the rendering below is testable with a
//! fixture, never a live `org.templaros.Host`.

use crate::humanize::bytes as human_bytes;
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// What `templar update` needs from `org.templaros.Host.Updates1`.
#[async_trait]
pub trait UpdatesClient {
    async fn current(&self) -> Result<String>;
    async fn plan(&self, changes_json: &str) -> Result<String>;
    async fn history(&self) -> Result<String>;
    async fn transaction(&self, id: &str) -> Result<String>;
}

#[async_trait]
impl UpdatesClient for templar_client::proxies::HostUpdatesProxy<'_> {
    async fn current(&self) -> Result<String> {
        self.current().await.context("Host.Updates1.Current")
    }
    async fn plan(&self, changes_json: &str) -> Result<String> {
        self.plan(changes_json).await.context("Host.Updates1.Plan")
    }
    async fn history(&self) -> Result<String> {
        self.history().await.context("Host.Updates1.History")
    }
    async fn transaction(&self, id: &str) -> Result<String> {
        self.transaction(id)
            .await
            .context("Host.Updates1.Transaction")
    }
}

/// What `templar update` needs from `org.templaros.Host.Deployments1`.
#[async_trait]
pub trait DeploymentsClient {
    async fn backend(&self) -> Result<String>;
    async fn list(&self) -> Result<String>;
    async fn stage(&self, id: &str, tries: u8) -> Result<()>;
    async fn mark_good(&self) -> Result<()>;
    async fn rollback(&self, id: &str) -> Result<()>;
}

#[async_trait]
impl DeploymentsClient for templar_client::proxies::HostDeploymentsProxy<'_> {
    async fn backend(&self) -> Result<String> {
        self.backend().await.context("Host.Deployments1.Backend")
    }
    async fn list(&self) -> Result<String> {
        self.list().await.context("Host.Deployments1.List")
    }
    async fn stage(&self, id: &str, tries: u8) -> Result<()> {
        self.stage(id, tries)
            .await
            .context("Host.Deployments1.Stage")
    }
    async fn mark_good(&self) -> Result<()> {
        self.mark_good().await.context("Host.Deployments1.MarkGood")
    }
    async fn rollback(&self, id: &str) -> Result<()> {
        self.rollback(id)
            .await
            .context("Host.Deployments1.Rollback")
    }
}

/// What `templar update` needs from `org.templaros.Host.Drivers1`.
#[async_trait]
pub trait DriversClient {
    async fn plan(&self) -> Result<String>;
    async fn violations(&self) -> Result<String>;
    async fn repair(&self) -> Result<String>;
}

#[async_trait]
impl DriversClient for templar_client::proxies::HostDriversProxy<'_> {
    async fn plan(&self) -> Result<String> {
        self.plan().await.context("Host.Drivers1.Plan")
    }
    async fn violations(&self) -> Result<String> {
        self.violations().await.context("Host.Drivers1.Violations")
    }
    async fn repair(&self) -> Result<String> {
        self.repair().await.context("Host.Drivers1.Repair")
    }
}

/// What `templar update` needs from `org.templaros.Host.Storage1`.
#[async_trait]
pub trait StorageClient {
    async fn trim_order(&self) -> Result<Vec<String>>;
    async fn usage(&self, mount_point: &str) -> Result<String>;
    async fn enforce(&self, mount_point: &str) -> Result<String>;
}

#[async_trait]
impl StorageClient for templar_client::proxies::HostStorageProxy<'_> {
    async fn trim_order(&self) -> Result<Vec<String>> {
        self.trim_order().await.context("Host.Storage1.TrimOrder")
    }
    async fn usage(&self, mount_point: &str) -> Result<String> {
        self.usage(mount_point).await.context("Host.Storage1.Usage")
    }
    async fn enforce(&self, mount_point: &str) -> Result<String> {
        self.enforce(mount_point)
            .await
            .context("Host.Storage1.Enforce")
    }
}

/// What `templar update` needs from `org.templaros.Host.Hardware1`.
#[async_trait]
pub trait HardwareClient {
    async fn graphics_profile(&self) -> Result<String>;
    async fn gpu_mode(&self) -> Result<String>;
    async fn set_gpu_mode(&self, mode: &str) -> Result<()>;
}

#[async_trait]
impl HardwareClient for templar_client::proxies::HostHardwareProxy<'_> {
    async fn graphics_profile(&self) -> Result<String> {
        self.graphics_profile()
            .await
            .context("Host.Hardware1.GraphicsProfile")
    }
    async fn gpu_mode(&self) -> Result<String> {
        self.gpu_mode().await.context("Host.Hardware1.GpuMode")
    }
    async fn set_gpu_mode(&self, mode: &str) -> Result<()> {
        self.set_gpu_mode(mode)
            .await
            .context("Host.Hardware1.SetGpuMode")
    }
}

/// Mirrors `templar_hostd::updates::ChangeSet`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ChangeSet {
    #[serde(default)]
    pub kernel: Option<String>,
    #[serde(default)]
    pub nvidia_modules: Option<String>,
    #[serde(default)]
    pub nvidia_userspace: Option<String>,
    #[serde(default)]
    pub firmware: Vec<String>,
    #[serde(default)]
    pub other_packages: Vec<String>,
    #[serde(default)]
    pub archive: Option<String>,
}

impl ChangeSet {
    /// One line naming what actually changes; the kernel and the NVIDIA modules are called out by
    /// name because they are the pair that must succeed or roll back together (ADR-0004).
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(kernel) = &self.kernel {
            parts.push(format!("kernel {kernel}"));
        }
        if let Some(modules) = &self.nvidia_modules {
            parts.push(format!("nvidia modules {modules}"));
        }
        if let Some(userspace) = &self.nvidia_userspace {
            parts.push(format!("nvidia userspace {userspace}"));
        }
        if !self.firmware.is_empty() {
            parts.push(format!("{} firmware package(s)", self.firmware.len()));
        }
        if !self.other_packages.is_empty() {
            parts.push(format!("{} other package(s)", self.other_packages.len()));
        }
        if parts.is_empty() {
            "no changes".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

/// Mirrors `templar_hostd::updates::Transaction`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transaction {
    pub id: String,
    #[serde(default)]
    pub changes: ChangeSet,
    pub phase: String,
    #[serde(default)]
    pub candidate_deployment: Option<String>,
    #[serde(default)]
    pub previous_deployment: Option<String>,
    #[serde(default)]
    pub log: Vec<String>,
}

/// `Current` answers with an empty string when nothing is in flight — not an error, and not a
/// transaction with an empty id.
pub fn parse_current(transaction_json: &str) -> Result<Option<Transaction>> {
    if transaction_json.trim().is_empty() || transaction_json.trim() == "null" {
        return Ok(None);
    }
    serde_json::from_str(transaction_json)
        .map(Some)
        .context("parsing Host.Updates1 transaction JSON")
}

pub fn parse_transactions(transactions_json: &str) -> Result<Vec<Transaction>> {
    if transactions_json.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(transactions_json).context("parsing Host.Updates1 transaction list JSON")
}

/// What a phase means for the machine in front of you, since the phase name alone does not say
/// whether a reboot is pending or whether the last one is still on trial.
pub fn describe_phase(phase: &str) -> &'static str {
    match phase {
        "planned" => "planned; nothing has been downloaded yet",
        "downloading" => "downloading packages",
        "staging" => "staging a new deployment",
        "verifying" => "verifying the staged deployment",
        "awaiting-reboot" => "staged and verified — reboot to try it",
        "trial" => "on trial: this boot decides whether it is promoted or rolled back",
        "promoted" => "promoted; this deployment is now the known-good one",
        "rolled-back" => "rolled back after a failed trial",
        "failed" => "failed",
        _ => "unknown phase",
    }
}

pub fn render_transaction(transaction: &Transaction) -> String {
    let mut out = vec![
        format!("transaction {}", transaction.id),
        format!(
            "  phase:     {} ({})",
            transaction.phase,
            describe_phase(&transaction.phase)
        ),
        format!("  changes:   {}", transaction.changes.summary()),
    ];
    if let Some(candidate) = &transaction.candidate_deployment {
        out.push(format!("  candidate: {candidate}"));
    }
    if let Some(previous) = &transaction.previous_deployment {
        out.push(format!("  previous:  {previous}"));
    }
    for line in &transaction.log {
        out.push(format!("  log:       {line}"));
    }
    out.join("\n")
}

pub fn render_transactions(transactions: &[Transaction]) -> String {
    if transactions.is_empty() {
        return "no transactions recorded".to_owned();
    }
    let rows: Vec<Vec<String>> = transactions
        .iter()
        .map(|transaction| {
            vec![
                transaction.id.clone(),
                transaction.phase.clone(),
                transaction.changes.summary(),
            ]
        })
        .collect();
    crate::format::table(&rows)
}

/// Mirrors `templar_hostd::deploy::Deployment`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Deployment {
    pub id: String,
    pub subvolume: String,
    pub esp_dir: String,
    #[serde(default)]
    pub kernel: Option<String>,
    #[serde(default)]
    pub nvidia_modules: Option<String>,
    pub known_good: bool,
}

pub fn parse_deployments(deployments_json: &str) -> Result<Vec<Deployment>> {
    if deployments_json.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(deployments_json).context("parsing Host.Deployments1 list JSON")
}

pub fn render_deployments(backend: &str, deployments: &[Deployment]) -> String {
    if deployments.is_empty() {
        return format!("boot backend: {backend}\nno deployments");
    }
    let mut rows = vec![vec![
        "id".to_owned(),
        "kernel".to_owned(),
        "nvidia".to_owned(),
        "state".to_owned(),
    ]];
    for deployment in deployments {
        rows.push(vec![
            deployment.id.clone(),
            deployment.kernel.clone().unwrap_or_else(|| "-".to_owned()),
            deployment
                .nvidia_modules
                .clone()
                .unwrap_or_else(|| "-".to_owned()),
            if deployment.known_good {
                "known-good".to_owned()
            } else {
                "untried".to_owned()
            },
        ]);
    }
    format!("boot backend: {backend}\n{}", crate::format::table(&rows))
}

/// Mirrors `templar_hostd::drivers::SupportViolation`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum SupportViolation {
    StandaloneInstaller {
        path: String,
    },
    ThirdPartyRepository {
        name: String,
    },
    Dkms {
        package: String,
    },
    MixedFlavours,
    VersionLockMismatch {
        component: String,
        expected: String,
        installed: String,
    },
}

impl SupportViolation {
    /// One sentence saying what is wrong and, implicitly, what to remove — these are the states
    /// that make a graphics stack unsupportable, so a code alone would be useless here.
    pub fn describe(&self) -> String {
        match self {
            Self::StandaloneInstaller { path } => format!(
                "NVIDIA's standalone installer left {path} behind; every file must be owned by a package"
            ),
            Self::ThirdPartyRepository { name } => {
                format!("the third-party driver repository {name} is enabled")
            }
            Self::Dkms { package } => {
                format!("{package} builds its modules with DKMS; this repository ships LRM only")
            }
            Self::MixedFlavours => {
                "both the open and the proprietary NVIDIA module flavours are present".to_owned()
            }
            Self::VersionLockMismatch {
                component,
                expected,
                installed,
            } => format!("{component} is {installed} but the locked set expects {expected}"),
        }
    }
}

/// Mirrors `templar_hostd::drivers::DriverPlan`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DriverPlan {
    #[serde(default)]
    pub metapackage: Option<String>,
    pub default_mode: String,
    #[serde(default)]
    pub violations: Vec<SupportViolation>,
}

pub fn parse_driver_plan(plan_json: &str) -> Result<DriverPlan> {
    serde_json::from_str(plan_json).context("parsing Host.Drivers1 plan JSON")
}

pub fn parse_violations(violations_json: &str) -> Result<Vec<SupportViolation>> {
    if violations_json.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(violations_json).context("parsing Host.Drivers1 violations JSON")
}

pub fn render_driver_plan(plan: &DriverPlan) -> String {
    let mut out = vec![
        format!(
            "metapackage:  {}",
            plan.metapackage
                .as_deref()
                .unwrap_or("(none — no NVIDIA GPU)")
        ),
        format!("default mode: {}", plan.default_mode),
    ];
    if plan.violations.is_empty() {
        out.push("support:      no violations".to_owned());
    } else {
        out.push(format!(
            "support:      {} violation(s)",
            plan.violations.len()
        ));
        for violation in &plan.violations {
            out.push(format!("  - {}", violation.describe()));
        }
    }
    out.join("\n")
}

/// Mirrors `templar_hostd::storage::VolumeUsage`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VolumeUsage {
    pub mount_point: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub reserved_bytes: u64,
}

pub fn parse_usage(usage_json: &str) -> Result<VolumeUsage> {
    serde_json::from_str(usage_json).context("parsing Host.Storage1 usage JSON")
}

pub fn render_usage(usage: &VolumeUsage) -> String {
    let usable = usage.free_bytes.saturating_sub(usage.reserved_bytes);
    format!(
        "{}\n  total:    {}\n  free:     {}\n  reserved: {} (login, update and recovery)\n  usable:   {}",
        usage.mount_point,
        human_bytes(usage.total_bytes),
        human_bytes(usage.free_bytes),
        human_bytes(usage.reserved_bytes),
        human_bytes(usable),
    )
}

/// Mirrors `templar_hostd::drivers::VersionLock`, `Repair`'s return value: the newly written locked
/// baseline.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct VersionLock {
    #[serde(default)]
    pub kernel: Option<String>,
    #[serde(default)]
    pub nvidia_lrm: Option<String>,
    #[serde(default)]
    pub nvidia_userspace: Option<String>,
}

pub fn parse_version_lock(lock_json: &str) -> Result<VersionLock> {
    serde_json::from_str(lock_json).context("parsing Host.Drivers1.Repair VersionLock JSON")
}

pub fn render_version_lock(lock: &VersionLock) -> String {
    format!(
        "locked baseline — kernel: {}, nvidia LRM: {}, nvidia userspace: {}",
        lock.kernel.as_deref().unwrap_or("(unversioned)"),
        lock.nvidia_lrm.as_deref().unwrap_or("(none)"),
        lock.nvidia_userspace.as_deref().unwrap_or("(none)"),
    )
}

/// Mirrors `templar_hostd::storage::TrimReport`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrimReport {
    pub target: String,
    pub freed_bytes: u64,
}

pub fn parse_trim_reports(reports_json: &str) -> Result<Vec<TrimReport>> {
    if reports_json.trim().is_empty() {
        return Ok(Vec::new());
    }
    serde_json::from_str(reports_json).context("parsing Host.Storage1.Enforce report JSON")
}

pub fn render_trim_reports(reports: &[TrimReport]) -> String {
    if reports.is_empty() {
        return "the reserve was already healthy; nothing was trimmed".to_owned();
    }
    reports
        .iter()
        .map(|report| {
            format!(
                "{}: freed {}",
                report.target,
                human_bytes(report.freed_bytes)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Mirrors `templar_hwdetect::Gpu`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gpu {
    pub pci_slot: String,
    pub vendor: String,
    #[serde(default)]
    pub boot_vga: bool,
    #[serde(default)]
    pub driver: Option<String>,
}

/// Mirrors `templar_hwdetect::GraphicsProfile`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GraphicsProfile {
    pub gpus: Vec<Gpu>,
    pub hybrid: bool,
    #[serde(default)]
    pub nvidia_flavour: Option<String>,
    #[serde(default)]
    pub supported_modes: Vec<String>,
}

pub fn parse_graphics_profile(profile_json: &str) -> Result<GraphicsProfile> {
    serde_json::from_str(profile_json).context("parsing Host.Hardware1.GraphicsProfile JSON")
}

pub fn render_graphics_profile(profile: &GraphicsProfile, gpu_mode: &str) -> String {
    let mut out = vec![
        format!(
            "declared mode: {}",
            if gpu_mode.is_empty() {
                "(unset — using the hardware default)"
            } else {
                gpu_mode
            }
        ),
        format!("hybrid graphics: {}", profile.hybrid),
        format!(
            "nvidia flavour: {}",
            profile
                .nvidia_flavour
                .as_deref()
                .unwrap_or("(no NVIDIA GPU)")
        ),
        format!("supported modes: {}", profile.supported_modes.join(", ")),
    ];
    for gpu in &profile.gpus {
        out.push(format!(
            "  {} {}{}{}",
            gpu.pci_slot,
            gpu.vendor,
            if gpu.boot_vga { " (boot VGA)" } else { "" },
            gpu.driver
                .as_ref()
                .map_or_else(String::new, |driver| format!(" — driver {driver}")),
        ));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRANSACTION: &str = r#"{
        "id":"20260821T143200Z",
        "changes":{"kernel":"7.0.0-14","nvidia_modules":"580.65.06","nvidia_userspace":"580.65.06",
                   "firmware":[],"other_packages":["libnvidia-egl-wayland1"]},
        "phase":"awaiting-reboot",
        "candidate_deployment":"@deployments/20260821T143200Z",
        "previous_deployment":"@deployments/20260814T090000Z",
        "log":["planned","staged"]
    }"#;

    #[test]
    fn a_transaction_parses_and_explains_its_phase() {
        let transaction = parse_current(TRANSACTION).unwrap().expect("a transaction");
        assert_eq!(transaction.phase, "awaiting-reboot");
        let rendered = render_transaction(&transaction);
        assert!(rendered.contains("kernel 7.0.0-14"));
        assert!(rendered.contains("nvidia modules 580.65.06"));
        assert!(rendered.contains("reboot to try it"));
        assert!(rendered.contains("@deployments/20260821T143200Z"));
    }

    #[test]
    fn an_idle_host_reports_no_transaction_rather_than_an_error() {
        assert_eq!(parse_current("").unwrap(), None);
        assert_eq!(parse_current("  ").unwrap(), None);
        assert_eq!(parse_current("null").unwrap(), None);
    }

    #[test]
    fn a_changeset_summary_names_the_kernel_and_driver_pair() {
        let changes = ChangeSet {
            kernel: Some("7.0.0-14".into()),
            nvidia_modules: Some("580.65.06".into()),
            ..ChangeSet::default()
        };
        assert_eq!(
            changes.summary(),
            "kernel 7.0.0-14, nvidia modules 580.65.06"
        );
        assert_eq!(ChangeSet::default().summary(), "no changes");
    }

    #[test]
    fn transaction_history_renders_a_row_each_and_says_when_empty() {
        let transactions = parse_transactions(&format!("[{TRANSACTION}]")).unwrap();
        assert_eq!(transactions.len(), 1);
        assert!(render_transactions(&transactions).contains("20260821T143200Z"));
        assert_eq!(render_transactions(&[]), "no transactions recorded");
        assert!(parse_transactions("").unwrap().is_empty());
    }

    #[test]
    fn deployments_render_with_their_known_good_state() {
        let json = r#"[
            {"id":"a","subvolume":"@deployments/a","esp_dir":"/boot/efi/a","kernel":"7.0.0-14",
             "nvidia_modules":"580.65.06","known_good":true},
            {"id":"b","subvolume":"@deployments/b","esp_dir":"/boot/efi/b","kernel":null,
             "nvidia_modules":null,"known_good":false}
        ]"#;
        let deployments = parse_deployments(json).unwrap();
        assert_eq!(deployments.len(), 2);
        let rendered = render_deployments("systemd-boot", &deployments);
        assert!(rendered.contains("boot backend: systemd-boot"));
        assert!(rendered.contains("known-good"));
        assert!(rendered.contains("untried"));
        assert!(rendered.contains('-'), "a null kernel renders as a dash");
    }

    #[test]
    fn every_support_violation_reads_as_a_sentence() {
        let json = r#"[
            {"kind":"standalone-installer","path":"/usr/bin/nvidia-uninstall"},
            {"kind":"third-party-repository","name":"graphics-drivers PPA"},
            {"kind":"dkms","package":"nvidia-dkms-580"},
            {"kind":"mixed-flavours"},
            {"kind":"version-lock-mismatch","component":"nvidia-modules","expected":"580.65.06","installed":"575.00.00"}
        ]"#;
        let violations = parse_violations(json).unwrap();
        assert_eq!(violations.len(), 5);
        let described: Vec<String> = violations.iter().map(SupportViolation::describe).collect();
        assert!(described[0].contains("nvidia-uninstall"));
        assert!(described[1].contains("graphics-drivers PPA"));
        assert!(described[2].contains("DKMS"));
        assert!(described[3].contains("both the open and the proprietary"));
        assert!(described[4].contains("575.00.00") && described[4].contains("580.65.06"));
    }

    #[test]
    fn a_driver_plan_renders_its_violations_or_says_there_are_none() {
        let clean = parse_driver_plan(
            r#"{"metapackage":"templaros-nvidia-open","default_mode":"hybrid","violations":[]}"#,
        )
        .unwrap();
        assert!(render_driver_plan(&clean).contains("no violations"));

        let dirty = parse_driver_plan(
            r#"{"metapackage":null,"default_mode":"integrated",
                 "violations":[{"kind":"mixed-flavours"}]}"#,
        )
        .unwrap();
        let rendered = render_driver_plan(&dirty);
        assert!(rendered.contains("(none — no NVIDIA GPU)"));
        assert!(rendered.contains("1 violation(s)"));
    }

    #[test]
    fn usage_shows_what_is_actually_usable_after_the_reserve() {
        let usage = parse_usage(
            r#"{"mount_point":"/","total_bytes":1073741824,"free_bytes":536870912,
                 "reserved_bytes":268435456}"#,
        )
        .unwrap();
        let rendered = render_usage(&usage);
        assert!(rendered.contains("total:    1.0 GiB"));
        assert!(rendered.contains("reserved: 256.0 MiB"));
        assert!(rendered.contains("usable:   256.0 MiB"));
    }

    #[test]
    fn a_reserve_larger_than_free_space_saturates_to_zero_usable() {
        let usage = VolumeUsage {
            mount_point: "/".into(),
            total_bytes: 100,
            free_bytes: 10,
            reserved_bytes: 50,
        };
        assert!(render_usage(&usage).contains("usable:   0 B"));
    }

    #[test]
    fn a_version_lock_names_every_component() {
        let lock = parse_version_lock(
            r#"{"kernel":"7.0.0-14","nvidia_lrm":"580.65.06","nvidia_userspace":"580.65.06"}"#,
        )
        .unwrap();
        let rendered = render_version_lock(&lock);
        assert!(rendered.contains("7.0.0-14"));
        assert!(rendered.contains("580.65.06"));

        assert!(render_version_lock(&VersionLock::default()).contains("(unversioned)"));
    }

    #[test]
    fn trim_reports_show_what_was_freed_or_that_nothing_needed_to_be() {
        let reports =
            parse_trim_reports(r#"[{"target":"apt-cache","freed_bytes":734003200}]"#).unwrap();
        assert!(render_trim_reports(&reports).contains("apt-cache: freed"));
        assert_eq!(
            render_trim_reports(&[]),
            "the reserve was already healthy; nothing was trimmed"
        );
        assert!(parse_trim_reports("").unwrap().is_empty());
    }

    #[test]
    fn a_graphics_profile_lists_every_gpu_and_the_declared_mode() {
        let profile = parse_graphics_profile(
            r#"{"gpus":[{"pci_slot":"0000:01:00.0","vendor":"nvidia","boot_vga":false,
                "driver":"nvidia"}],"hybrid":true,"nvidia_flavour":"open",
                "supported_modes":["hybrid","discrete"]}"#,
        )
        .unwrap();
        let rendered = render_graphics_profile(&profile, "hybrid");
        assert!(rendered.contains("declared mode: hybrid"));
        assert!(rendered.contains("0000:01:00.0"));
        assert!(rendered.contains("driver nvidia"));

        assert!(render_graphics_profile(&profile, "").contains("unset"));
    }
}
