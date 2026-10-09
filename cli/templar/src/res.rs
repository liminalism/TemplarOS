//! `templar res` — the resource governor: pressure level, containment stage, per-instance victim
//! scores with their breakdowns, and the observe-only trace ring (`docs/architecture/resources.md`).
//!
//! Every D-Bus call goes through [`ResourcesClient`] so the rendering below is testable with a
//! fixture, never a live `org.templaros.Resources`.

use crate::humanize::{bytes as human_bytes, duration_secs, timestamp_ms as human_time_utc};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// What `templar res` needs from `org.templaros.Resources1`.
#[async_trait]
pub trait ResourcesClient {
    async fn status(&self) -> Result<(String, bool)>;
    async fn targets(&self) -> Result<String>;
    async fn explain_target(&self, resource_id: &str) -> Result<String>;
    async fn stage(&self) -> Result<String>;
    async fn set_stage(&self, stage: &str) -> Result<()>;
    async fn recent_traces(&self, limit: u32) -> Result<String>;
}

#[async_trait]
impl ResourcesClient for templar_client::proxies::ResourcesProxy<'_> {
    async fn status(&self) -> Result<(String, bool)> {
        self.status().await.context("Resources1.Status")
    }
    async fn targets(&self) -> Result<String> {
        self.targets().await.context("Resources1.Targets")
    }
    async fn explain_target(&self, resource_id: &str) -> Result<String> {
        self.explain_target(resource_id)
            .await
            .context("Resources1.ExplainTarget")
    }
    async fn stage(&self) -> Result<String> {
        self.stage().await.context("Resources1.Stage")
    }
    async fn set_stage(&self, stage: &str) -> Result<()> {
        self.set_stage(stage).await.context("Resources1.SetStage")
    }
    async fn recent_traces(&self, limit: u32) -> Result<String> {
        self.recent_traces(limit)
            .await
            .context("Resources1.RecentTraces")
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PsiSample {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    #[serde(default)]
    pub total_microseconds: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Psi {
    pub some: PsiSample,
    pub full: PsiSample,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ScoreBreakdown {
    pub memory_term: f64,
    pub growth_term: f64,
    pub psi_term: f64,
    pub disposable_term: f64,
    pub visible_term: f64,
    pub foreground_term: f64,
    pub unsaved_term: f64,
    pub total: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InstanceReport {
    pub instance_id: String,
    #[serde(flatten)]
    pub metadata: templar_types::ResourceMetadata,
    pub memory_current_bytes: u64,
    pub memory_growth_bytes_per_second: i64,
    pub full_psi_avg10: f64,
    pub foreground: bool,
    pub visible: bool,
    pub protected: bool,
    pub disposable: bool,
    pub has_unsaved_work: bool,
    pub victim_score: f64,
    pub score_breakdown: ScoreBreakdown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub stage: String,
    #[serde(default)]
    pub uptime_seconds: u64,
    #[serde(default)]
    pub mem_available_bytes: u64,
    #[serde(default)]
    pub global_memory_psi: Psi,
    #[serde(default)]
    pub global_io_psi: Psi,
    #[serde(default)]
    pub instances: Vec<InstanceReport>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Explanation {
    pub instance_id: String,
    pub known: bool,
    pub level: String,
    pub stage: String,
    #[serde(default)]
    pub protected: Option<bool>,
    #[serde(default)]
    pub victim_score: Option<f64>,
    #[serde(default)]
    pub score_breakdown: Option<ScoreBreakdown>,
    #[serde(default)]
    pub rank: Option<usize>,
    pub candidate_count: usize,
    pub would_be_selected: bool,
    #[serde(default)]
    pub top_candidate_app_id: Option<String>,
    pub summary: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionTrace {
    pub kind: String,
    pub instance_id: String,
    pub applied: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TraceRecord {
    pub timestamp_ms: i64,
    pub level: String,
    pub stage: String,
    pub mem_available_bytes: u64,
    #[serde(default)]
    pub actions: Vec<ActionTrace>,
    #[serde(default)]
    pub victim: Option<String>,
}

/// Parse `Resources1.Report`'s JSON, or `Report::default()` (`level: ""`) for the "before the
/// first tick" empty-object case the daemon documents.
pub fn parse_report(report_json: &str) -> Result<Report> {
    if report_json.trim() == "{}" {
        return Ok(Report::default());
    }
    serde_json::from_str(report_json).context("parsing Resources1.Report JSON")
}

pub fn render_report(report: &Report) -> String {
    if report.level.is_empty() {
        return "no report yet (the governor has not completed its first tick)".to_owned();
    }
    let mut out = format!(
        "pressure: {}   stage: {}   uptime: {}   available: {}\n",
        report.level,
        report.stage,
        duration_secs(report.uptime_seconds),
        human_bytes(report.mem_available_bytes),
    );
    if report.instances.is_empty() {
        out.push_str("no tracked instances\n");
        return out;
    }
    let mut ranked: Vec<_> = report.instances.iter().collect();
    ranked.sort_by(|left, right| {
        right
            .victim_score
            .partial_cmp(&left.victim_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for instance in &ranked {
        let _ = writeln!(
            out,
            "{:<28} score {:>7.1}  mem {:>9}  growth {:>8}/s  psi {:>5.1}{}{}{}{}",
            instance.metadata.identity,
            instance.victim_score,
            human_bytes(instance.memory_current_bytes),
            human_bytes(instance.memory_growth_bytes_per_second.max(0) as u64),
            instance.full_psi_avg10,
            if instance.protected {
                "  protected"
            } else {
                ""
            },
            if instance.foreground {
                "  foreground"
            } else {
                ""
            },
            if instance.visible { "  visible" } else { "" },
            if instance.has_unsaved_work {
                "  unsaved-work"
            } else {
                ""
            },
        );
        let _ = writeln!(
            out,
            "  {:<26} breakdown: memory {:+.1}  growth {:+.1}  psi {:+.1}  disposable {:+.1}  visible {:+.1}  foreground {:+.1}  unsaved {:+.1}",
            "",
            instance.score_breakdown.memory_term,
            instance.score_breakdown.growth_term,
            instance.score_breakdown.psi_term,
            instance.score_breakdown.disposable_term,
            instance.score_breakdown.visible_term,
            instance.score_breakdown.foreground_term,
            instance.score_breakdown.unsaved_term,
        );
    }
    out
}

pub fn render_explanation(explanation: &Explanation) -> String {
    explanation.summary.clone()
}

pub fn render_traces(traces: &[TraceRecord]) -> String {
    if traces.is_empty() {
        return "no traces recorded yet".to_owned();
    }
    let mut out = String::new();
    for trace in traces {
        let _ = writeln!(
            out,
            "{}  {:<9} stage={:<12} available={}",
            human_time_utc(trace.timestamp_ms),
            trace.level,
            trace.stage,
            human_bytes(trace.mem_available_bytes),
        );
        for action in &trace.actions {
            let _ = writeln!(
                out,
                "  {} {} on {} — {}",
                if action.applied {
                    "[applied]"
                } else {
                    "[decided]"
                },
                action.kind,
                action.instance_id,
                action.reason,
            );
        }
    }
    out
}

/// Parse `RecentTraces`'s newline-separated JSON lines (newest first).
pub fn parse_traces(traces_json: &str) -> Vec<TraceRecord> {
    traces_json
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
