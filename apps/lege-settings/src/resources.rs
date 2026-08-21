//! Resources page (`org.legeos.Resources1`): the containment stage, and "the traces that justify
//! raising it" (`docs/architecture/resources.md`).
//!
//! [`Report`]/[`InstanceReport`]/[`ScoreBreakdown`]/[`Psi`]/[`PsiSample`] mirror
//! `services/lege-resourced/src/state.rs` and `psi.rs`'s wire shapes exactly (field names and
//! `#[serde(rename_all = "kebab-case")]` enum spellings); [`TraceRecord`]/[`ActionTrace`] mirror
//! `services/lege-resourced/src/trace.rs`. `Report()` answers `"{}"` before the governor's first
//! tick, and `RecentTraces` returns newline-separated JSON objects, newest first.

use anyhow::{Context, Result};
use serde::Deserialize;

/// Only `avg10` is rendered; `avg60`/`avg300` exist on the wire but this page has no use for
/// them yet (extra JSON fields are ignored by `serde` on deserialize, so dropping them here does
/// not require `#[serde(deny_unknown_fields)]` gymnastics).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct PsiSample {
    pub(crate) avg10: f64,
}

/// Only `full` (full-stall PSI) is rendered; `some` exists on the wire but a partial stall is not
/// part of what this page currently explains.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct Psi {
    #[serde(default)]
    pub(crate) full: PsiSample,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub(crate) struct ScoreBreakdown {
    pub(crate) memory_term: f64,
    pub(crate) growth_term: f64,
    pub(crate) psi_term: f64,
    pub(crate) disposable_term: f64,
    pub(crate) visible_term: f64,
    pub(crate) foreground_term: f64,
    pub(crate) unsaved_term: f64,
    pub(crate) total: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InstanceReport {
    pub(crate) instance_id: String,
    pub(crate) app_id: String,
    pub(crate) memory_current_bytes: u64,
    pub(crate) memory_growth_bytes_per_second: i64,
    pub(crate) full_psi_avg10: f64,
    pub(crate) foreground: bool,
    pub(crate) visible: bool,
    pub(crate) protected: bool,
    pub(crate) disposable: bool,
    pub(crate) has_unsaved_work: bool,
    pub(crate) victim_score: f64,
    pub(crate) score_breakdown: ScoreBreakdown,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct Report {
    #[serde(default)]
    pub(crate) level: String,
    #[serde(default)]
    pub(crate) stage: String,
    #[serde(default)]
    pub(crate) uptime_seconds: u64,
    #[serde(default)]
    pub(crate) mem_available_bytes: u64,
    #[serde(default)]
    pub(crate) global_memory_psi: Psi,
    #[serde(default)]
    pub(crate) global_io_psi: Psi,
    #[serde(default)]
    pub(crate) instances: Vec<InstanceReport>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ActionTrace {
    pub(crate) kind: String,
    pub(crate) instance_id: String,
    pub(crate) applied: bool,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct TraceRecord {
    pub(crate) timestamp_ms: i64,
    pub(crate) level: String,
    pub(crate) stage: String,
    pub(crate) mem_available_bytes: u64,
    #[serde(default)]
    pub(crate) actions: Vec<ActionTrace>,
}

/// The bus operations this page needs.
pub(crate) trait ResourcesSource {
    /// `(level, observe_only)`.
    async fn status(&self) -> Result<(String, bool)>;
    async fn report(&self) -> Result<String>;
    async fn recent_traces(&self, limit: u32) -> Result<String>;
}

impl ResourcesSource for lege_client::proxies::ResourcesProxy<'_> {
    async fn status(&self) -> Result<(String, bool)> {
        self.status().await.context("Resources1.Status")
    }

    async fn report(&self) -> Result<String> {
        self.report().await.context("Resources1.Report")
    }

    async fn recent_traces(&self, limit: u32) -> Result<String> {
        self.recent_traces(limit)
            .await
            .context("Resources1.RecentTraces")
    }
}

fn render_status(level: &str, observe_only: bool) -> String {
    format!(
        "pressure: {level}   governor: {}",
        if observe_only {
            "observe-only (decides and logs, performs nothing)"
        } else {
            "enforcing"
        }
    )
}

fn render_report(report: &Report) -> String {
    if report.level.is_empty() {
        return "No report yet (the governor has not completed its first tick).".to_owned();
    }
    let mut lines = vec![format!(
        "stage: {}   uptime: {}s   available: {} MiB   full memory PSI avg10: {:.1}   full I/O PSI avg10: {:.1}",
        report.stage,
        report.uptime_seconds,
        report.mem_available_bytes / (1024 * 1024),
        report.global_memory_psi.full.avg10,
        report.global_io_psi.full.avg10,
    )];
    if report.instances.is_empty() {
        lines.push("No tracked instances.".to_owned());
    } else {
        let mut ranked = report.instances.clone();
        ranked.sort_by(|left, right| {
            right
                .victim_score
                .partial_cmp(&left.victim_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for instance in &ranked {
            lines.push(format!(
                "  {} [{}] — score {:.1} ({} MiB, growth {} B/s, psi {:.1}){}{}{}{}{}",
                instance.app_id,
                instance.instance_id,
                instance.victim_score,
                instance.memory_current_bytes / (1024 * 1024),
                instance.memory_growth_bytes_per_second,
                instance.full_psi_avg10,
                if instance.protected {
                    ", protected"
                } else {
                    ""
                },
                if instance.disposable {
                    ", disposable"
                } else {
                    ""
                },
                if instance.foreground {
                    ", foreground"
                } else {
                    ""
                },
                if instance.visible { ", visible" } else { "" },
                if instance.has_unsaved_work {
                    ", unsaved work"
                } else {
                    ""
                },
            ));
            let breakdown = &instance.score_breakdown;
            lines.push(format!(
                "      breakdown: memory {:+.1}  growth {:+.1}  psi {:+.1}  disposable {:+.1}  visible {:+.1}  foreground {:+.1}  unsaved {:+.1}  = {:.1}",
                breakdown.memory_term,
                breakdown.growth_term,
                breakdown.psi_term,
                breakdown.disposable_term,
                breakdown.visible_term,
                breakdown.foreground_term,
                breakdown.unsaved_term,
                breakdown.total,
            ));
        }
    }
    lines.join("\n")
}

fn render_traces(raw: &str, limit: u32) -> String {
    let traces: Vec<TraceRecord> = raw
        .lines()
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| serde_json::from_str(line).ok())
        .take(limit as usize)
        .collect();
    if traces.is_empty() {
        return "No traces recorded yet.".to_owned();
    }
    let mut lines = vec!["Recent traces (what justified the current stage):".to_owned()];
    for trace in &traces {
        lines.push(format!(
            "  [{}ms] {} / {} — {} MiB available",
            trace.timestamp_ms,
            trace.level,
            trace.stage,
            trace.mem_available_bytes / (1024 * 1024),
        ));
        for action in &trace.actions {
            lines.push(format!(
                "    {} {} on {} — {}",
                if action.applied {
                    "[applied]"
                } else {
                    "[decided]"
                },
                action.kind,
                action.instance_id,
                action.reason,
            ));
        }
    }
    lines.join("\n")
}

pub(crate) async fn render_with<S: ResourcesSource>(source: &S, trace_limit: u32) -> String {
    let status = match source.status().await {
        Ok((level, observe_only)) => render_status(&level, observe_only),
        Err(error) => return format!("service unavailable: {error:#}"),
    };

    let report = match source.report().await {
        Ok(json) => match serde_json::from_str::<Report>(&json) {
            Ok(report) => render_report(&report),
            Err(_) if json.trim() == "{}" => render_report(&Report::default()),
            Err(error) => format!("could not parse Resources1.Report: {error}"),
        },
        Err(error) => format!("could not fetch Resources1.Report: {error:#}"),
    };

    let traces = match source.recent_traces(trace_limit).await {
        Ok(raw) => render_traces(&raw, trace_limit),
        Err(error) => format!("could not fetch Resources1.RecentTraces: {error:#}"),
    };

    [status, report, traces].join("\n\n")
}

pub(crate) async fn render(connection: &zbus::Connection, trace_limit: u32) -> String {
    match lege_client::proxies::ResourcesProxy::new(connection).await {
        Ok(proxy) => render_with(&proxy, trace_limit).await,
        Err(error) => format!("service unavailable: could not reach Resources1 ({error})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_REPORT: &str = r#"{
        "level": "severe", "stage": "observe-only", "uptime_seconds": 120,
        "mem_available_bytes": 2147483648,
        "global_memory_psi": {"some": {"avg10": 1.0}, "full": {"avg10": 0.7}},
        "global_io_psi": {"some": {"avg10": 0.0}, "full": {"avg10": 0.0}},
        "instances": [
            {"instance_id": "11112222-3333-4444-5555-666677778888", "app_id": "org.legeos.Waterfox",
             "memory_current_bytes": 14395793408, "memory_growth_bytes_per_second": 1048576,
             "full_psi_avg10": 2.4, "foreground": false, "visible": true, "protected": false,
             "disposable": false, "has_unsaved_work": false, "victim_score": 42.5,
             "score_breakdown": {"memory_term": 30.0, "growth_term": 1.0, "psi_term": 9.6,
                                  "disposable_term": 0.0, "visible_term": -20.0,
                                  "foreground_term": 0.0, "unsaved_term": 0.0, "total": 42.5}}
        ]
    }"#;

    struct FakeResources {
        status: (String, bool),
        report: String,
        traces: String,
    }

    impl ResourcesSource for FakeResources {
        async fn status(&self) -> Result<(String, bool)> {
            Ok(self.status.clone())
        }
        async fn report(&self) -> Result<String> {
            Ok(self.report.clone())
        }
        async fn recent_traces(&self, _limit: u32) -> Result<String> {
            Ok(self.traces.clone())
        }
    }

    #[tokio::test]
    async fn render_with_shows_the_ranked_instance_and_its_score() {
        let fake = FakeResources {
            status: ("severe".to_owned(), true),
            report: SAMPLE_REPORT.to_owned(),
            traces: String::new(),
        };
        let rendered = render_with(&fake, 10).await;
        assert!(rendered.contains("pressure: severe"));
        assert!(rendered.contains("observe-only"));
        assert!(rendered.contains("org.legeos.Waterfox"));
        assert!(rendered.contains("score 42.5"));
        assert!(rendered.contains("No traces recorded yet."));
    }

    #[tokio::test]
    async fn render_with_handles_the_before_first_tick_empty_report() {
        let fake = FakeResources {
            status: ("normal".to_owned(), true),
            report: "{}".to_owned(),
            traces: String::new(),
        };
        let rendered = render_with(&fake, 10).await;
        assert!(rendered.contains("No report yet"));
    }

    #[tokio::test]
    async fn render_with_renders_traces_with_their_actions() {
        let fake = FakeResources {
            status: ("critical".to_owned(), false),
            report: "{}".to_owned(),
            traces: r#"{"timestamp_ms":1000,"level":"critical","stage":"terminate","mem_available_bytes":1048576,"actions":[{"kind":"kill-cgroup","instance_id":"aaaa","applied":true,"reason":"persistent critical pressure"}]}"#.to_owned(),
        };
        let rendered = render_with(&fake, 10).await;
        assert!(rendered.contains("enforcing"));
        assert!(rendered.contains("[applied] kill-cgroup on aaaa"));
        assert!(rendered.contains("persistent critical pressure"));
    }

    #[tokio::test]
    async fn render_with_degrades_on_a_status_failure() {
        struct Failing;
        impl ResourcesSource for Failing {
            async fn status(&self) -> Result<(String, bool)> {
                anyhow::bail!("no reply from Resources1")
            }
            async fn report(&self) -> Result<String> {
                Ok("{}".to_owned())
            }
            async fn recent_traces(&self, _limit: u32) -> Result<String> {
                Ok(String::new())
            }
        }
        let rendered = render_with(&Failing, 10).await;
        assert!(rendered.starts_with("service unavailable:"));
    }
}
