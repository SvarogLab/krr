//! A variant that also recommends a CPU limit, from a higher percentile.
//!
//! Port of `robusta_krr/strategies/simple_limit.py`.

use serde::{Deserialize, Serialize};

use crate::model::{ByResource, K8sObject, RecValue, ResourceType};
use crate::prometheus::MetricsData;
use crate::prometheus::metrics::Metric;

use super::{
    CommonSettings, Strategy, StrategyRecommendation, all_values, memory_recommendation,
    percentile, precondition, series,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimpleLimitSettings {
    #[serde(flatten)]
    pub common: CommonSettings,
    /// Percentile of CPU usage to request.
    pub cpu_request: f64,
    /// Percentile of CPU usage to allow as a limit.
    pub cpu_limit: f64,
}

pub struct SimpleLimitStrategy {
    pub settings: SimpleLimitSettings,
}

impl SimpleLimitStrategy {
    pub fn new(settings: SimpleLimitSettings) -> anyhow::Result<Self> {
        settings.common.validate()?;
        for (name, value) in [
            ("cpu_request", settings.cpu_request),
            ("cpu_limit", settings.cpu_limit),
        ] {
            if value <= 0.0 || value > 100.0 {
                anyhow::bail!("{name} must be within (0, 100]");
            }
        }
        Ok(SimpleLimitStrategy { settings })
    }
}

impl Strategy for SimpleLimitStrategy {
    fn name(&self) -> &'static str {
        "simple_limit"
    }

    fn description(&self) -> String {
        let settings = &self.settings;
        let mut description = format!(
            "CPU request: {}% percentile, limit: {}% percentile\n\
             Memory request: max + {buffer}%, limit: max + {buffer}%\n\
             History: {history} hours\n\
             Step: {step} minutes\n\n\
             All parameters can be customized. For example: \
             `krr simple_limit --cpu-request 66 --cpu-limit 96 --history-duration 24 --timeframe-duration 0.5`\n",
            settings.cpu_request,
            settings.cpu_limit,
            buffer = settings.common.memory_buffer_percentage,
            history = settings.common.history_duration,
            step = settings.common.timeframe_duration,
        );

        if !settings.common.allow_hpa {
            description.push_str(
                "\nThis strategy does not work with objects with HPA defined (Horizontal Pod Autoscaler).\n\
                 If HPA is defined for CPU or Memory, the strategy will return \"?\" for that resource.\n\
                 You can override this behaviour by passing the --allow-hpa flag\n",
            );
        }

        description.push_str("\nLearn more: https://github.com/robusta-dev/krr#algorithm");
        description
    }

    fn common(&self) -> &CommonSettings {
        &self.settings.common
    }

    fn metrics(&self) -> Vec<Metric> {
        let mut metrics = vec![
            Metric::Cpu,
            Metric::MaxMemory,
            Metric::CpuAmount,
            Metric::MemoryAmount,
        ];
        if self.settings.common.use_oomkill_data {
            metrics.push(Metric::MaxOomKilledMemory);
        }
        metrics
    }

    fn run(&self, data: &MetricsData, object: &K8sObject) -> ByResource<StrategyRecommendation> {
        let common = &self.settings.common;

        let cpu = precondition(
            data,
            Metric::Cpu,
            Metric::CpuAmount,
            object,
            ResourceType::Cpu,
            common,
        )
        .unwrap_or_else(|| {
            let values = all_values(series(data, Metric::Cpu).expect("checked above"));
            StrategyRecommendation {
                request: RecValue::from_f64(percentile(&values, self.settings.cpu_request)),
                limit: RecValue::from_f64(percentile(&values, self.settings.cpu_limit)),
                info: None,
            }
        });

        let memory = precondition(
            data,
            Metric::MaxMemory,
            Metric::MemoryAmount,
            object,
            ResourceType::Memory,
            common,
        )
        .unwrap_or_else(|| memory_recommendation(data, object, common));

        ByResource { cpu, memory }
    }

    fn settings_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.settings).expect("settings are serializable")
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{assert_close, data, enough_points, object};
    use super::*;

    fn settings() -> SimpleLimitSettings {
        SimpleLimitSettings {
            common: CommonSettings::default(),
            cpu_request: 66.0,
            cpu_limit: 96.0,
        }
    }

    fn metrics(cpu: Vec<f64>) -> MetricsData {
        let mut entries = vec![
            (Metric::Cpu, vec![("web-1", cpu)]),
            (Metric::MaxMemory, vec![("web-1", vec![100.0])]),
        ];
        entries.extend(enough_points(100.0));
        data(entries)
    }

    #[test]
    fn rejects_impossible_settings() {
        assert!(
            SimpleLimitStrategy::new(SimpleLimitSettings {
                cpu_request: 0.0,
                ..settings()
            })
            .is_err()
        );
        assert!(
            SimpleLimitStrategy::new(SimpleLimitSettings {
                cpu_limit: 101.0,
                ..settings()
            })
            .is_err()
        );
    }

    #[test]
    fn requests_and_limits_come_from_two_percentiles() {
        let data = metrics(vec![1.0, 2.0, 3.0, 4.0]);
        let result = SimpleLimitStrategy::new(settings())
            .unwrap()
            .run(&data, &object());

        // numpy.percentile([1,2,3,4], 66) == 2.98, ... 96) == 3.88
        let request = result[ResourceType::Cpu].request.as_f64().unwrap();
        let limit = result[ResourceType::Cpu].limit.as_f64().unwrap();
        assert!((request - 2.98).abs() < 1e-9, "{request}");
        assert!((limit - 3.88).abs() < 1e-9, "{limit}");
    }

    #[test]
    fn memory_behaves_like_the_simple_strategy() {
        let data = metrics(vec![1.0]);
        let result = SimpleLimitStrategy::new(settings())
            .unwrap()
            .run(&data, &object());
        assert_close(result[ResourceType::Memory].request, 115.0);
        assert_close(result[ResourceType::Memory].limit, 115.0);
    }

    #[test]
    fn reports_missing_data() {
        let result = SimpleLimitStrategy::new(settings())
            .unwrap()
            .run(&MetricsData::new(), &object());
        assert_eq!(
            result[ResourceType::Cpu],
            StrategyRecommendation::undefined("No data")
        );
    }

    #[test]
    fn loads_the_raw_cpu_series() {
        let strategy = SimpleLimitStrategy::new(settings()).unwrap();
        assert!(strategy.metrics().contains(&Metric::Cpu));
        assert!(
            !strategy
                .metrics()
                .iter()
                .any(|m| matches!(m, Metric::PercentileCpu(_)))
        );

        let json = strategy.settings_json();
        assert_eq!(json["cpu_request"], serde_json::json!(66.0));
        assert_eq!(json["cpu_limit"], serde_json::json!(96.0));
    }
}
