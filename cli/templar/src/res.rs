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
    async fn report(&self) -> Result<String>;
    async fn explain_instance(&self, instance_id: &str) -> Result<String>;
    async fn stage(&self) -> Result<String>;
    async fn set_stage(&self, stage: &str) -> Result<()>;
    async fn recent_traces(&self, limit: u32) -> Result<String>;
}

#[async_trait]
impl ResourcesClient for templar_client::proxies::ResourcesProxy<'_> {
    async fn status(&self) -> Result<(String, bool)> {
        self.status().await.context("Resources1.Status")
    }
    async fn report(&self) -> Result<String> {
        self.report().await.context("Resources1.Report")
    }
    async fn explain_instance(&self, instance_id: &str) -> Result<String> {
        self.explain_instance(instance_id)
            .await
            .context("Resources1.ExplainInstance")
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
    pub app_id: String,
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
    let mut ranked = report.instances.clone();
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
            instance.app_id,
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

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_REPORT: &str = r#"{
        "level": "severe",
        "stage": "observe-only",
        "uptime_seconds": 3725,
        "mem_available_bytes": 2147483648,
        "global_memory_psi": {"some": {"avg10":0.0,"avg60":0.0,"avg300":0.0,"total_microseconds":0},
                               "full": {"avg10":0.7,"avg60":0.5,"avg300":0.3,"total_microseconds":100}},
        "global_io_psi": {"some": {"avg10":0.0,"avg60":0.0,"avg300":0.0,"total_microseconds":0},
                           "full": {"avg10":0.0,"avg60":0.0,"avg300":0.0,"total_microseconds":0}},
        "instances": [
            {"instance_id":"11112222-3333-4444-5555-666677778888","app_id":"org.templaros.Waterfox",
             "memory_current_bytes":14395793408,"memory_growth_bytes_per_second":1048576,
             "full_psi_avg10":2.4,"foreground":false,"visible":true,"protected":false,
             "disposable":false,"has_unsaved_work":false,"victim_score":42.5,
             "score_breakdown":{"memory_term":30.0,"growth_term":1.0,"psi_term":9.6,
                                 "disposable_term":0.0,"visible_term":-20.0,"foreground_term":0.0,
                                 "unsaved_term":0.0,"total":42.5}}
        ]
    }"#;

    #[test]
    fn empty_object_parses_as_no_report_yet() {
        let report = parse_report("{}").unwrap();
        assert!(report.level.is_empty());
        assert_eq!(
            render_report(&report),
            "no report yet (the governor has not completed its first tick)"
        );
    }

    #[test]
    fn a_real_report_renders_the_ranked_instance_with_its_breakdown() {
        let report = parse_report(SAMPLE_REPORT).unwrap();
        assert_eq!(report.level, "severe");
        assert_eq!(report.instances.len(), 1);
        let rendered = render_report(&report);
        assert!(rendered.contains("pressure: severe"));
        assert!(rendered.contains("org.templaros.Waterfox"));
        assert!(rendered.contains("visible"));
        assert!(rendered.contains("breakdown:"));
    }

    #[test]
    fn traces_parse_from_newline_delimited_json_newest_first() {
        let lines = format!(
            "{}\n{}\n",
            r#"{"timestamp_ms":1000,"level":"normal","stage":"observe-only","mem_available_bytes":1,"instances":[],"victim":null,"actions":[]}"#,
            r#"{"timestamp_ms":2000,"level":"critical","stage":"terminate","mem_available_bytes":2,"instances":[],"victim":"aaaa","actions":[{"kind":"kill-cgroup","instance_id":"aaaa","applied":true,"reason":"persistent pressure"}]}"#,
        );
        let traces = parse_traces(&lines);
        assert_eq!(traces.len(), 2);
        assert_eq!(traces[1].level, "critical");
        let rendered = render_traces(&traces);
        assert!(rendered.contains("kill-cgroup"));
        assert!(rendered.contains("[applied]"));
    }

    #[test]
    fn no_traces_says_so_plainly() {
        assert_eq!(render_traces(&[]), "no traces recorded yet");
    }
}
