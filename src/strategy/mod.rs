//! Recommendation strategies and the calculations they share.
//!
//! Port of `robusta_krr/core/abstract/strategies.py` and `robusta_krr/strategies/*`.

mod simple;
mod simple_limit;

use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

pub use simple::SimpleStrategy;
pub use simple_limit::SimpleLimitStrategy;

use crate::model::{ByResource, K8sObject, RecValue, ResourceType};
use crate::prometheus::metrics::Metric;
use crate::prometheus::{MetricsData, PodSeries};

/// What a strategy proposes for one resource.
#[derive(Debug, Clone, PartialEq)]
pub struct StrategyRecommendation {
    pub request: RecValue,
    pub limit: RecValue,
    pub info: Option<String>,
}

impl StrategyRecommendation {
    /// The strategy could not make a recommendation; both values become `?`.
    pub fn undefined(info: &str) -> Self {
        StrategyRecommendation {
            request: RecValue::Unknown,
            limit: RecValue::Unknown,
            info: Some(info.to_string()),
        }
    }
}

/// Settings shared by all strategies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommonSettings {
    /// Length of the history to look at, in hours.
    pub history_duration: f64,
    /// Resolution of the history, in minutes.
    pub timeframe_duration: f64,
    /// Headroom added on top of peak memory usage, in percent.
    pub memory_buffer_percentage: f64,
    /// Data points needed before a recommendation is made.
    pub points_required: u32,
    /// Recommend even for workloads driven by an HPA.
    pub allow_hpa: bool,
    /// Raise the memory recommendation when OOMKills were seen.
    pub use_oomkill_data: bool,
    /// Headroom added on top of an OOMKilled limit, in percent.
    pub oom_memory_buffer_percentage: f64,
}

impl Default for CommonSettings {
    fn default() -> Self {
        CommonSettings {
            history_duration: 24.0 * 7.0 * 2.0,
            timeframe_duration: 1.25,
            memory_buffer_percentage: 15.0,
            points_required: 100,
            allow_hpa: false,
            use_oomkill_data: false,
            oom_memory_buffer_percentage: 25.0,
        }
    }
}

impl CommonSettings {
    pub fn validate(&self) -> Result<()> {
        if self.history_duration < 1.0 {
            bail!("history_duration must be at least 1 hour");
        }
        if self.timeframe_duration <= 0.0 {
            bail!("timeframe_duration must be greater than 0 minutes");
        }
        if self.memory_buffer_percentage <= 0.0 {
            bail!("memory_buffer_percentage must be greater than 0");
        }
        if self.points_required < 1 {
            bail!("points_required must be at least 1");
        }
        if self.oom_memory_buffer_percentage < 0.0 {
            bail!("oom_memory_buffer_percentage cannot be negative");
        }
        Ok(())
    }

    pub fn history(&self) -> Duration {
        Duration::from_secs_f64(self.history_duration * 3600.0)
    }

    pub fn timeframe(&self) -> Duration {
        Duration::from_secs_f64(self.timeframe_duration * 60.0)
    }

    /// Upstream requires at least three hours of history for the simple strategies.
    pub fn history_range_enough(&self, range: Duration) -> bool {
        range >= Duration::from_secs(3 * 3600)
    }
}

/// A recommendation strategy.
pub trait Strategy: Send + Sync {
    fn name(&self) -> &'static str;

    /// Human readable summary shown above the table.
    fn description(&self) -> String;

    fn common(&self) -> &CommonSettings;

    /// Metrics that have to be loaded before [`Strategy::run`] can be called.
    fn metrics(&self) -> Vec<Metric>;

    fn run(&self, data: &MetricsData, object: &K8sObject) -> ByResource<StrategyRecommendation>;

    /// Settings as they appear in the report.
    fn settings_json(&self) -> serde_json::Value;
}

/// Builds a strategy by name; `args` are the strategy specific CLI arguments.
pub fn find(name: &str, args: &StrategyArgs) -> Result<Box<dyn Strategy>> {
    match name {
        "simple" => Ok(Box::new(SimpleStrategy::new(simple::SimpleSettings {
            common: args.common.clone(),
            cpu_percentile: args.cpu_percentile,
        })?)),
        "simple_limit" => Ok(Box::new(SimpleLimitStrategy::new(
            simple_limit::SimpleLimitSettings {
                common: args.common.clone(),
                cpu_request: args.cpu_request,
                cpu_limit: args.cpu_limit,
            },
        )?)),
        _ => bail!(
            "Unknown strategy name: {name}. Available strategies: {}",
            names().join(", ")
        ),
    }
}

pub fn names() -> Vec<&'static str> {
    vec!["simple", "simple_limit"]
}

/// Strategy arguments as accepted on the command line.
#[derive(Debug, Clone, PartialEq)]
pub struct StrategyArgs {
    pub common: CommonSettings,
    pub cpu_percentile: f64,
    pub cpu_request: f64,
    pub cpu_limit: f64,
}

impl Default for StrategyArgs {
    fn default() -> Self {
        StrategyArgs {
            common: CommonSettings::default(),
            cpu_percentile: 95.0,
            cpu_request: 66.0,
            cpu_limit: 96.0,
        }
    }
}

// --------------------------------------------------------------------------------------
// Calculations shared by the strategies
// --------------------------------------------------------------------------------------

/// Series of a metric, or an empty map when it was not loaded.
pub fn series(data: &MetricsData, metric: Metric) -> Option<&PodSeries> {
    data.get(metric.key()).filter(|series| !series.is_empty())
}

/// Number of data points a metric reported, summed over the pods.
///
/// The amount loaders return a single point per pod whose value is the count.
pub fn total_points(data: &MetricsData, metric: Metric) -> f64 {
    match series(data, metric) {
        None => 0.0,
        Some(series) => series
            .values()
            .filter_map(|points| points.first())
            .map(|(_, value)| value)
            .sum(),
    }
}

/// Every value of every pod, as one flat list.
pub fn all_values(series: &PodSeries) -> Vec<f64> {
    series
        .values()
        .flat_map(|points| points.iter().map(|(_, value)| *value))
        .collect()
}

/// The largest value of any pod, or `None` when there are no samples.
fn max_value(series: &PodSeries) -> Option<f64> {
    all_values(series).into_iter().reduce(f64::max)
}

/// numpy's default (linear interpolation) percentile, so recommendations match upstream.
pub fn percentile(values: &[f64], percentile: f64) -> f64 {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);

    let rank = percentile / 100.0 * (values.len() - 1) as f64;
    let low = rank.floor() as usize;
    let high = rank.ceil() as usize;
    if low == high {
        return values[low];
    }
    values[low] + (values[high] - values[low]) * (rank - low as f64)
}

/// Common preconditions: no data, too few points, or an HPA driving this resource.
pub fn precondition(
    data: &MetricsData,
    metric: Metric,
    amount_metric: Metric,
    object: &K8sObject,
    resource: ResourceType,
    settings: &CommonSettings,
) -> Option<StrategyRecommendation> {
    if series(data, metric).is_none() {
        return Some(StrategyRecommendation::undefined("No data"));
    }

    if total_points(data, amount_metric) < settings.points_required as f64 {
        return Some(StrategyRecommendation::undefined("Not enough data"));
    }

    let driven_by_hpa = object
        .hpa
        .as_ref()
        .and_then(|hpa| hpa.target_for(resource))
        .is_some();
    if driven_by_hpa && !settings.allow_hpa {
        return Some(StrategyRecommendation::undefined("HPA detected"));
    }

    None
}

/// Peak memory plus a buffer, never below the largest OOMKilled limit plus its own buffer.
///
/// Identical in both strategies upstream.
pub fn memory_recommendation(
    data: &MetricsData,
    _object: &K8sObject,
    settings: &CommonSettings,
) -> StrategyRecommendation {
    let max_oomkill = if settings.use_oomkill_data {
        series(data, Metric::MaxOomKilledMemory)
            .and_then(|series| {
                // the loader reports a single point per pod
                series
                    .values()
                    .filter_map(|points| points.first())
                    .map(|(_, value)| *value)
                    .reduce(f64::max)
            })
            .unwrap_or(0.0)
    } else {
        0.0
    };

    let peak = series(data, Metric::MaxMemory)
        .and_then(max_value)
        .unwrap_or(f64::NAN);
    let usage = f64::max(
        peak * (1.0 + settings.memory_buffer_percentage / 100.0),
        max_oomkill * (1.0 + settings.oom_memory_buffer_percentage / 100.0),
    );

    StrategyRecommendation {
        request: RecValue::from_f64(usage),
        limit: RecValue::from_f64(usage),
        info: (max_oomkill != 0.0).then(|| "OOMKill detected".to_string()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{HpaData, Kind, PodData, ResourceAllocations};
    use std::collections::BTreeMap;

    pub fn object() -> K8sObject {
        K8sObject {
            cluster: None,
            name: "web".into(),
            container: "app".into(),
            pods: vec![PodData {
                name: "web-1".into(),
                deleted: false,
            }],
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

    pub fn hpa(cpu: Option<f64>, memory: Option<f64>) -> HpaData {
        HpaData {
            min_replicas: Some(1),
            max_replicas: 10,
            current_replicas: Some(1),
            desired_replicas: 1,
            target_cpu_utilization_percentage: cpu,
            target_memory_utilization_percentage: memory,
        }
    }

    /// Test fixture shape: one metric with its per-pod values.
    pub type MetricFixture<'a> = (Metric, Vec<(&'a str, Vec<f64>)>);

    /// Builds metric data: `[(metric, [(pod, [values])])]`, timestamps are filled in.
    pub fn data(entries: Vec<MetricFixture>) -> MetricsData {
        let mut data = MetricsData::new();
        for (metric, pods) in entries {
            let mut series = BTreeMap::new();
            for (pod, values) in pods {
                let points = values
                    .into_iter()
                    .enumerate()
                    .map(|(i, value)| (1000.0 + i as f64, value))
                    .collect();
                series.insert(pod.to_string(), points);
            }
            data.insert(metric.key().to_string(), series);
        }
        data
    }

    /// Compares a recommended value, tolerating binary floating point noise
    /// (`200 * 1.15` is 229.99999999999997 in both python and rust).
    pub fn assert_close(value: RecValue, expected: f64) {
        let actual = value
            .as_f64()
            .unwrap_or_else(|| panic!("expected a number, got {value:?}"));
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, got {actual}"
        );
    }

    /// Amount-loader entries reporting `count` data points for both resources.
    pub fn enough_points(count: f64) -> Vec<MetricFixture<'static>> {
        vec![
            (Metric::CpuAmount, vec![("web-1", vec![count])]),
            (Metric::MemoryAmount, vec![("web-1", vec![count])]),
        ]
    }

    #[test]
    fn settings_are_validated() {
        assert!(CommonSettings::default().validate().is_ok());

        let invalid = CommonSettings {
            history_duration: 0.5,
            ..Default::default()
        };
        assert!(invalid.validate().is_err());

        let invalid = CommonSettings {
            timeframe_duration: 0.0,
            ..Default::default()
        };
        assert!(invalid.validate().is_err());

        let invalid = CommonSettings {
            points_required: 0,
            ..Default::default()
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn durations_come_from_hours_and_minutes() {
        let settings = CommonSettings {
            history_duration: 2.0,
            timeframe_duration: 1.25,
            ..Default::default()
        };
        assert_eq!(settings.history(), Duration::from_secs(7200));
        assert_eq!(settings.timeframe(), Duration::from_secs(75));
    }

    #[test]
    fn three_hours_of_history_are_enough() {
        let settings = CommonSettings::default();
        assert!(!settings.history_range_enough(Duration::from_secs(2 * 3600)));
        assert!(settings.history_range_enough(Duration::from_secs(3 * 3600)));
    }

    #[test]
    fn percentiles_interpolate_like_numpy() {
        let values = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&values, 0.0), 1.0);
        assert_eq!(percentile(&values, 100.0), 4.0);
        assert_eq!(percentile(&values, 50.0), 2.5);
        // numpy.percentile([1,2,3,4], 66) == 2.98
        assert!((percentile(&values, 66.0) - 2.98).abs() < 1e-9);
        assert_eq!(percentile(&[5.0], 50.0), 5.0);
    }

    #[test]
    fn points_are_summed_over_pods() {
        let data = data(vec![(
            Metric::CpuAmount,
            vec![("web-1", vec![60.0]), ("web-2", vec![40.0])],
        )]);
        assert_eq!(total_points(&data, Metric::CpuAmount), 100.0);
        assert_eq!(total_points(&data, Metric::MemoryAmount), 0.0);
    }

    #[test]
    fn values_of_all_pods_are_concatenated() {
        let data = data(vec![(
            Metric::Cpu,
            vec![("web-1", vec![1.0, 2.0]), ("web-2", vec![3.0])],
        )]);
        let mut values = all_values(series(&data, Metric::Cpu).unwrap());
        values.sort_by(f64::total_cmp);
        assert_eq!(values, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn preconditions_report_missing_data() {
        let settings = CommonSettings::default();
        let empty = MetricsData::new();
        let result = precondition(
            &empty,
            Metric::Cpu,
            Metric::CpuAmount,
            &object(),
            ResourceType::Cpu,
            &settings,
        );
        assert_eq!(result, Some(StrategyRecommendation::undefined("No data")));
    }

    #[test]
    fn preconditions_report_too_few_points() {
        let settings = CommonSettings::default();
        let data = data(vec![
            (Metric::Cpu, vec![("web-1", vec![1.0])]),
            (Metric::CpuAmount, vec![("web-1", vec![99.0])]),
        ]);
        let result = precondition(
            &data,
            Metric::Cpu,
            Metric::CpuAmount,
            &object(),
            ResourceType::Cpu,
            &settings,
        );
        assert_eq!(
            result,
            Some(StrategyRecommendation::undefined("Not enough data"))
        );
    }

    #[test]
    fn preconditions_report_hpa_unless_allowed() {
        let data = data(vec![
            (Metric::Cpu, vec![("web-1", vec![1.0])]),
            (Metric::CpuAmount, vec![("web-1", vec![100.0])]),
        ]);
        let mut object = object();
        object.hpa = Some(hpa(Some(80.0), None));

        let settings = CommonSettings::default();
        let result = precondition(
            &data,
            Metric::Cpu,
            Metric::CpuAmount,
            &object,
            ResourceType::Cpu,
            &settings,
        );
        assert_eq!(
            result,
            Some(StrategyRecommendation::undefined("HPA detected"))
        );

        // an HPA on cpu does not block the memory recommendation
        let result = precondition(
            &data,
            Metric::Cpu,
            Metric::CpuAmount,
            &object,
            ResourceType::Memory,
            &settings,
        );
        assert_eq!(result, None);

        let allowed = CommonSettings {
            allow_hpa: true,
            ..Default::default()
        };
        let result = precondition(
            &data,
            Metric::Cpu,
            Metric::CpuAmount,
            &object,
            ResourceType::Cpu,
            &allowed,
        );
        assert_eq!(result, None);
    }

    #[test]
    fn memory_recommendation_adds_a_buffer_to_the_peak() {
        let data = data(vec![
            (
                Metric::MaxMemory,
                vec![("web-1", vec![100.0, 200.0]), ("web-2", vec![150.0])],
            ),
            (Metric::MemoryAmount, vec![("web-1", vec![100.0])]),
        ]);
        let recommendation = memory_recommendation(&data, &object(), &CommonSettings::default());

        assert_close(recommendation.request, 230.0);
        assert_close(recommendation.limit, 230.0);
        assert_eq!(recommendation.info, None);
    }

    #[test]
    fn memory_recommendation_respects_oomkills() {
        let data = data(vec![
            (Metric::MaxMemory, vec![("web-1", vec![100.0])]),
            (Metric::MemoryAmount, vec![("web-1", vec![100.0])]),
            (Metric::MaxOomKilledMemory, vec![("web-1", vec![1000.0])]),
        ]);
        let settings = CommonSettings {
            use_oomkill_data: true,
            ..Default::default()
        };
        let recommendation = memory_recommendation(&data, &object(), &settings);

        assert_close(recommendation.request, 1250.0);
        assert_eq!(recommendation.info.as_deref(), Some("OOMKill detected"));

        // ignored unless the flag is set
        let recommendation = memory_recommendation(&data, &object(), &CommonSettings::default());
        assert_close(recommendation.request, 115.0);
    }

    #[test]
    fn strategy_lookup() {
        let args = StrategyArgs::default();
        assert_eq!(find("simple", &args).unwrap().name(), "simple");
        assert_eq!(find("simple_limit", &args).unwrap().name(), "simple_limit");
        assert!(find("nope", &args).is_err());
        assert_eq!(names(), vec!["simple", "simple_limit"]);
    }
}
