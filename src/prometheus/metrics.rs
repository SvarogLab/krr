//! The PromQL queries KRR runs, one per metric loader.
//!
//! Port of `robusta_krr/core/integrations/prometheus/metrics/*`.

use std::time::Duration;

use crate::model::K8sObject;

/// Prometheus endpoint a metric is read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryType {
    /// `/api/v1/query`: a single value per series.
    Instant,
    /// `/api/v1/query_range`: a series over time.
    Range,
}

/// A metric loader. The variants mirror the loader classes upstream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Metric {
    /// CPU usage at the given percentile (0..100).
    PercentileCpu(f64),
    /// CPU usage over time.
    Cpu,
    /// Number of CPU data points, used to decide whether there is enough data.
    CpuAmount,
    /// Peak memory usage.
    MaxMemory,
    /// Number of memory data points.
    MemoryAmount,
    /// Memory limit of containers that were OOMKilled.
    MaxOomKilledMemory,
}

impl Metric {
    /// Key the metric is stored under, matching the upstream loader class names.
    pub fn key(self) -> &'static str {
        match self {
            Metric::PercentileCpu(_) => "PercentileCPULoader",
            Metric::Cpu => "CPULoader",
            Metric::CpuAmount => "CPUAmountLoader",
            Metric::MaxMemory => "MaxMemoryLoader",
            Metric::MemoryAmount => "MemoryAmountLoader",
            Metric::MaxOomKilledMemory => "MaxOOMKilledMemoryLoader",
        }
    }

    pub fn query_type(self) -> QueryType {
        match self {
            Metric::Cpu => QueryType::Range,
            _ => QueryType::Instant,
        }
    }

    /// Whether a missing result is worth a warning.
    pub fn warn_on_no_data(self) -> bool {
        self != Metric::MaxOomKilledMemory
    }

    /// The PromQL query for one workload.
    ///
    /// `duration` and `step` are PromQL duration literals, `cluster` is the
    /// centralized-Prometheus label fragment (may be empty).
    pub fn query(self, object: &K8sObject, duration: &str, step: &str, cluster: &str) -> String {
        let selector = workload_selector(object, cluster);
        let cpu_usage = format!(
            "max(rate(container_cpu_usage_seconds_total{{{selector}}}[{step}])) by (container, pod, job)"
        );
        let memory_usage = format!(
            "max(container_memory_working_set_bytes{{{selector}}}) by (container, pod, job)"
        );

        match self {
            Metric::Cpu => cpu_usage,
            Metric::PercentileCpu(percentile) => {
                // upstream rounds the quantile to two decimals
                let quantile = (percentile / 100.0 * 100.0).round() / 100.0;
                format!("quantile_over_time({quantile}, {cpu_usage}[{duration}:{step}])")
            }
            Metric::CpuAmount => {
                let samples = format!(
                    "max(container_cpu_usage_seconds_total{{{selector}}}) by (container, pod, job)"
                );
                format!("count_over_time({samples}[{duration}:{step}])")
            }
            Metric::MaxMemory => format!("max_over_time({memory_usage}[{duration}:{step}])"),
            Metric::MemoryAmount => format!("count_over_time({memory_usage}[{duration}:{step}])"),
            Metric::MaxOomKilledMemory => {
                let limits = format!(
                    "max(kube_pod_container_resource_limits{{resource=\"memory\", {selector}}}) by (pod, container, job)"
                );
                let oomkilled = format!(
                    "max(kube_pod_container_status_last_terminated_reason{{reason=\"OOMKilled\", {selector}}}) by (pod, container, job, reason)"
                );
                format!(
                    "max_over_time(max({limits} * on(pod, container, job) group_left(reason) {oomkilled}) by (container, pod, job)[{duration}:{step}])"
                )
            }
        }
    }
}

/// Label matchers selecting the containers of one workload.
fn workload_selector(object: &K8sObject, cluster: &str) -> String {
    format!(
        "namespace=\"{}\", pod=~\"{}\", container=\"{}\"{cluster}",
        object.namespace,
        pods_selector(object),
        object.container
    )
}

/// Pod name alternation for a PromQL `pod=~".."` matcher.
pub fn pods_selector(object: &K8sObject) -> String {
    object
        .pods
        .iter()
        .map(|pod| pod.name.as_str())
        .collect::<Vec<_>>()
        .join("|")
}

/// Prometheus refuses a range query that would return more points than this per series.
pub const MAX_RANGE_POINTS: u64 = 11_000;

/// Widens the step until a range query fits in the point budget.
///
/// The default history of two weeks at a 1.25 minute step asks for 16k points, which every
/// Prometheus rejects with "exceeded maximum resolution".
pub fn range_step(period: Duration, step: Duration) -> Duration {
    let points = period.as_secs_f64() / step.as_secs_f64();
    if points <= MAX_RANGE_POINTS as f64 {
        return step;
    }

    let seconds = (period.as_secs_f64() / MAX_RANGE_POINTS as f64).ceil();
    Duration::from_secs_f64(seconds)
}

/// Formats a step for PromQL: whole seconds.
pub fn step_literal(step: Duration) -> String {
    format!("{}s", step.as_secs_f64().round() as u64)
}

/// Formats a period for PromQL: whole days if longer than a day, whole minutes otherwise.
pub fn duration_literal(period: Duration) -> String {
    let seconds = period.as_secs();
    if seconds > 60 * 60 * 24 {
        format!("{}d", seconds / (60 * 60 * 24))
    } else {
        format!("{}m", seconds / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, PodData, ResourceAllocations};

    fn object() -> K8sObject {
        K8sObject {
            cluster: None,
            name: "web".into(),
            container: "app".into(),
            pods: vec![
                PodData {
                    name: "web-1".into(),
                    deleted: false,
                },
                PodData {
                    name: "web-2".into(),
                    deleted: true,
                },
            ],
            hpa: None,
            namespace: "default".into(),
            kind: Kind::Deployment,
            allocations: ResourceAllocations::default(),
            warnings: Default::default(),
            labels: None,
            annotations: None,
            pod_selector: None,
            uid: None,
            grouped_jobs: Vec::new(),
        }
    }

    #[test]
    fn keys_match_upstream_loader_names() {
        assert_eq!(Metric::PercentileCpu(95.0).key(), "PercentileCPULoader");
        assert_eq!(Metric::Cpu.key(), "CPULoader");
        assert_eq!(Metric::CpuAmount.key(), "CPUAmountLoader");
        assert_eq!(Metric::MaxMemory.key(), "MaxMemoryLoader");
        assert_eq!(Metric::MemoryAmount.key(), "MemoryAmountLoader");
        assert_eq!(Metric::MaxOomKilledMemory.key(), "MaxOOMKilledMemoryLoader");
    }

    #[test]
    fn only_the_cpu_series_needs_a_range_query() {
        assert_eq!(Metric::Cpu.query_type(), QueryType::Range);
        assert_eq!(Metric::PercentileCpu(95.0).query_type(), QueryType::Instant);
        assert_eq!(Metric::MaxMemory.query_type(), QueryType::Instant);
    }

    #[test]
    fn oomkill_metric_does_not_warn_when_empty() {
        assert!(Metric::MaxMemory.warn_on_no_data());
        assert!(!Metric::MaxOomKilledMemory.warn_on_no_data());
    }

    #[test]
    fn selectors_list_every_pod_including_deleted_ones() {
        assert_eq!(pods_selector(&object()), "web-1|web-2");
    }

    #[test]
    fn duration_literals_follow_upstream_rounding() {
        assert_eq!(step_literal(Duration::from_secs(75)), "75s");
        assert_eq!(duration_literal(Duration::from_secs(3600)), "60m");
        assert_eq!(duration_literal(Duration::from_secs(24 * 3600)), "1440m");
        assert_eq!(duration_literal(Duration::from_secs(14 * 24 * 3600)), "14d");
    }

    #[test]
    fn range_steps_stay_inside_the_point_budget() {
        let fortnight = Duration::from_secs(14 * 24 * 3600);

        // the defaults ask for 16128 points, which prometheus rejects
        let step = range_step(fortnight, Duration::from_secs(75));
        assert_eq!(step, Duration::from_secs(110));
        assert!(fortnight.as_secs() / step.as_secs() <= MAX_RANGE_POINTS);

        // a step that already fits is left alone
        let step = Duration::from_secs(300);
        assert_eq!(range_step(fortnight, step), step);
        assert_eq!(
            range_step(Duration::from_secs(3600), Duration::from_secs(75)),
            Duration::from_secs(75)
        );
    }

    #[test]
    fn percentile_cpu_query() {
        let query = Metric::PercentileCpu(95.0).query(&object(), "14d", "75s", "");
        assert!(query.contains("quantile_over_time(0.95,"), "{query}");
        assert!(
            query.contains("rate(container_cpu_usage_seconds_total{"),
            "{query}"
        );
        assert!(query.contains(r#"namespace="default""#), "{query}");
        assert!(query.contains(r#"pod=~"web-1|web-2""#), "{query}");
        assert!(query.contains(r#"container="app""#), "{query}");
        assert!(query.contains("[75s]"), "{query}");
        assert!(query.contains("by (container, pod, job)"), "{query}");
        assert!(query.contains("[14d:75s]"), "{query}");
    }

    #[test]
    fn percentile_is_rounded_like_upstream() {
        let query = Metric::PercentileCpu(66.0).query(&object(), "14d", "75s", "");
        assert!(query.contains("quantile_over_time(0.66,"), "{query}");
    }

    #[test]
    fn cluster_label_is_appended_to_every_selector() {
        let cluster = r#", cluster="prod""#;
        for metric in [
            Metric::Cpu,
            Metric::CpuAmount,
            Metric::PercentileCpu(95.0),
            Metric::MaxMemory,
            Metric::MemoryAmount,
            Metric::MaxOomKilledMemory,
        ] {
            let query = metric.query(&object(), "14d", "75s", cluster);
            assert!(query.contains(r#"cluster="prod""#), "{:?}: {query}", metric);
        }
    }

    #[test]
    fn memory_queries_use_working_set_bytes() {
        let query = Metric::MaxMemory.query(&object(), "14d", "75s", "");
        assert!(query.starts_with("max_over_time("), "{query}");
        assert!(
            query.contains("container_memory_working_set_bytes{"),
            "{query}"
        );
        assert!(query.contains("[14d:75s]"), "{query}");

        let query = Metric::MemoryAmount.query(&object(), "14d", "75s", "");
        assert!(query.starts_with("count_over_time("), "{query}");
    }

    #[test]
    fn oomkill_query_joins_limits_with_the_termination_reason() {
        let query = Metric::MaxOomKilledMemory.query(&object(), "14d", "75s", "");
        assert!(
            query.contains("kube_pod_container_resource_limits{"),
            "{query}"
        );
        assert!(query.contains(r#"resource="memory""#), "{query}");
        assert!(
            query.contains("kube_pod_container_status_last_terminated_reason{"),
            "{query}"
        );
        assert!(query.contains(r#"reason="OOMKilled""#), "{query}");
        assert!(query.contains("group_left(reason)"), "{query}");
    }
}
