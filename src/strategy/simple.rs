//! The default strategy: a CPU percentile for requests, peak memory plus a buffer.
//!
//! Port of `robusta_krr/strategies/simple.py`.

use serde::{Deserialize, Serialize};

use crate::model::{ByResource, K8sObject, RecValue, ResourceType};
use crate::prometheus::MetricsData;
use crate::prometheus::metrics::Metric;

use super::{
    CommonSettings, Strategy, StrategyRecommendation, all_values, memory_recommendation,
    precondition, series,
};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimpleSettings {
    #[serde(flatten)]
    pub common: CommonSettings,
    /// Percentile of CPU usage to request.
    pub cpu_percentile: f64,
}

pub struct SimpleStrategy {
    pub settings: SimpleSettings,
}

impl SimpleStrategy {
    pub fn new(settings: SimpleSettings) -> anyhow::Result<Self> {
        settings.common.validate()?;
        if settings.cpu_percentile <= 0.0 || settings.cpu_percentile > 100.0 {
            anyhow::bail!("cpu_percentile must be within (0, 100]");
        }
        Ok(SimpleStrategy { settings })
    }
}

impl Strategy for SimpleStrategy {
    fn name(&self) -> &'static str {
        "simple"
    }

    fn description(&self) -> String {
        let settings = &self.settings;
        let mut description = format!(
            "CPU request: {}% percentile, limit: unset\n\
             Memory request: max + {buffer}%, limit: max + {buffer}%\n\
             History: {history} hours\n\
             Step: {step} minutes\n\n\
             All parameters can be customized. For example: \
             `krr simple --cpu-percentile 90 --memory-buffer-percentage 15 --history-duration 24 --timeframe-duration 0.5`\n",
            settings.cpu_percentile,
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
            Metric::PercentileCpu(self.settings.cpu_percentile),
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
        let cpu_metric = Metric::PercentileCpu(self.settings.cpu_percentile);

        let cpu = precondition(
            data,
            cpu_metric,
            Metric::CpuAmount,
            object,
            ResourceType::Cpu,
            common,
        )
        .unwrap_or_else(|| {
            // the percentile is computed by Prometheus, so the peak of the series is the request
            let usage = all_values(series(data, cpu_metric).expect("checked above"))
                .into_iter()
                .reduce(f64::max)
                .unwrap_or(f64::NAN);
            StrategyRecommendation {
                request: RecValue::from_f64(usage),
                limit: RecValue::Unset,
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
    use super::super::tests::{assert_close, data, enough_points, hpa, object};
    use super::*;

    fn strategy(settings: SimpleSettings) -> SimpleStrategy {
        SimpleStrategy::new(settings).unwrap()
    }

    fn settings() -> SimpleSettings {
        SimpleSettings {
            common: CommonSettings::default(),
            cpu_percentile: 95.0,
        }
    }

    fn metrics(cpu: Vec<f64>, memory: Vec<f64>, points: f64) -> MetricsData {
        let mut entries = vec![
            (Metric::PercentileCpu(95.0), vec![("web-1", cpu)]),
            (Metric::MaxMemory, vec![("web-1", memory)]),
        ];
        entries.extend(enough_points(points));
        data(entries)
    }

    #[test]
    fn rejects_impossible_settings() {
        let invalid = SimpleSettings {
            cpu_percentile: 0.0,
            ..settings()
        };
        assert!(SimpleStrategy::new(invalid).is_err());

        let invalid = SimpleSettings {
            cpu_percentile: 101.0,
            ..settings()
        };
        assert!(SimpleStrategy::new(invalid).is_err());
    }

    #[test]
    fn requests_the_cpu_percentile_and_leaves_the_limit_unset() {
        let data = metrics(vec![0.1, 0.25, 0.2], vec![100.0], 100.0);
        let result = strategy(settings()).run(&data, &object());

        assert_eq!(result[ResourceType::Cpu].request, RecValue::Val(0.25));
        assert_eq!(result[ResourceType::Cpu].limit, RecValue::Unset);
        assert_eq!(result[ResourceType::Cpu].info, None);
    }

    #[test]
    fn requests_and_limits_memory_with_a_buffer() {
        let data = metrics(vec![0.1], vec![100.0, 200.0], 100.0);
        let result = strategy(settings()).run(&data, &object());

        assert_close(result[ResourceType::Memory].request, 230.0);
        assert_close(result[ResourceType::Memory].limit, 230.0);
    }

    #[test]
    fn reports_missing_and_insufficient_data() {
        let empty = MetricsData::new();
        let result = strategy(settings()).run(&empty, &object());
        assert_eq!(
            result[ResourceType::Cpu],
            StrategyRecommendation::undefined("No data")
        );
        assert_eq!(
            result[ResourceType::Memory],
            StrategyRecommendation::undefined("No data")
        );

        let data = metrics(vec![0.1], vec![100.0], 99.0);
        let result = strategy(settings()).run(&data, &object());
        assert_eq!(
            result[ResourceType::Cpu],
            StrategyRecommendation::undefined("Not enough data")
        );
    }

    #[test]
    fn skips_resources_driven_by_an_hpa() {
        let data = metrics(vec![0.1], vec![100.0], 100.0);
        let mut object = object();
        object.hpa = Some(hpa(Some(80.0), None));

        let result = strategy(settings()).run(&data, &object);
        assert_eq!(
            result[ResourceType::Cpu],
            StrategyRecommendation::undefined("HPA detected")
        );
        assert_close(result[ResourceType::Memory].request, 115.0);
    }

    #[test]
    fn requests_the_configured_metrics() {
        let metrics = strategy(settings()).metrics();
        assert!(metrics.contains(&Metric::PercentileCpu(95.0)));
        assert!(metrics.contains(&Metric::MaxMemory));
        assert!(metrics.contains(&Metric::CpuAmount));
        assert!(metrics.contains(&Metric::MemoryAmount));
        assert!(!metrics.contains(&Metric::MaxOomKilledMemory));

        let with_oomkill = SimpleSettings {
            common: CommonSettings {
                use_oomkill_data: true,
                ..CommonSettings::default()
            },
            ..settings()
        };
        assert!(
            strategy(with_oomkill)
                .metrics()
                .contains(&Metric::MaxOomKilledMemory)
        );
    }

    #[test]
    fn describes_itself_and_its_settings() {
        let strategy = strategy(settings());
        let description = strategy.description();
        assert!(description.contains("95"), "{description}");
        assert!(description.contains("HPA"), "{description}");

        let json = strategy.settings_json();
        assert_eq!(json["cpu_percentile"], serde_json::json!(95.0));
        assert_eq!(json["history_duration"], serde_json::json!(336.0));
    }
}
