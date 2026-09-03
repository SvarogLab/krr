//! Scan results: recommendations, severities and the overall score.
//!
//! Port of `robusta_krr/core/models/result.py`.

use std::collections::BTreeMap;

use owo_colors::OwoColorize;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::model::{
    ByResource, K8sObject, RecValue, ResourceAllocations, ResourceType, Selector,
    format_recommendation_value,
};
use crate::severity::Severity;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    pub value: RecValue,
    pub severity: Severity,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceRecommendation {
    pub requests: ByResource<Recommendation>,
    pub limits: ByResource<Recommendation>,
    pub info: ByResource<Option<String>>,
}

impl ResourceRecommendation {
    pub fn get(&self, selector: Selector) -> &ByResource<Recommendation> {
        match selector {
            Selector::Requests => &self.requests,
            Selector::Limits => &self.limits,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceScan {
    pub object: K8sObject,
    pub recommended: ResourceRecommendation,
    pub severity: Severity,
}

impl ResourceScan {
    /// Compares the current allocations of an object with a recommendation.
    pub fn calculate(object: K8sObject, recommendation: ResourceAllocations) -> ResourceScan {
        let recommend = |selector: Selector| {
            ByResource::from_fn(|resource| {
                let current = object.allocations.get(selector)[resource];
                let recommended = recommendation.get(selector)[resource];
                Recommendation {
                    value: recommended,
                    severity: Severity::calculate(current, recommended, resource),
                }
            })
        };

        let recommended = ResourceRecommendation {
            requests: recommend(Selector::Requests),
            limits: recommend(Selector::Limits),
            info: recommendation.info.clone(),
        };

        let severity = Severity::BY_PRIORITY
            .into_iter()
            .find(|severity| {
                Selector::ALL.iter().any(|&selector| {
                    ResourceType::ALL
                        .iter()
                        .any(|&r| recommended.get(selector)[r].severity == *severity)
                })
            })
            .unwrap_or(Severity::Unknown);

        ResourceScan {
            object,
            recommended,
            severity,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StrategyData {
    pub name: String,
    pub settings: serde_json::Value,
}

/// The full report of a scan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanResult {
    pub scans: Vec<ResourceScan>,
    pub score: i64,
    pub resources: Vec<String>,
    pub description: Option<String>,
    pub strategy: StrategyData,
    #[serde(default)]
    pub errors: Vec<serde_json::Value>,
    #[serde(rename = "clusterSummary", default)]
    pub cluster_summary: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    pub config: Option<Config>,
}

impl ScanResult {
    pub fn new(scans: Vec<ResourceScan>, strategy: StrategyData, config: Config) -> ScanResult {
        ScanResult {
            score: ScanResult::calculate_score(&scans),
            scans,
            resources: ResourceType::ALL
                .iter()
                .map(|r| r.key().to_string())
                .collect(),
            description: None,
            strategy,
            errors: Vec::new(),
            cluster_summary: BTreeMap::new(),
            config: Some(config),
        }
    }

    /// `0..100`: the share of scans that are neither critical nor warnings.
    pub fn calculate_score(scans: &[ResourceScan]) -> i64 {
        if scans.is_empty() {
            return 0;
        }

        let cost: f64 = scans
            .iter()
            .map(|scan| match scan.severity {
                Severity::Warning => 0.7,
                Severity::Critical => 1.0,
                _ => 0.0,
            })
            .sum();

        ((scans.len() as f64 - cost) / scans.len() as f64 * 100.0) as i64
    }

    pub fn score_letter(&self) -> &'static str {
        match self.score {
            score if score < 30 => "F",
            score if score < 55 => "D",
            score if score < 70 => "C",
            score if score < 90 => "B",
            _ => "A",
        }
    }

    /// Configuration used for the scan, falling back to defaults for hand-written reports.
    pub fn config(&self) -> Config {
        self.config.clone().unwrap_or_default()
    }
}

/// Renders the change between the allocated and the recommended value, e.g. `-43m`.
///
/// Returns an empty string for limits and for values the strategy could not determine,
/// mirroring `format_diff` upstream.
pub fn format_diff(
    allocated: RecValue,
    recommended: RecValue,
    selector: Selector,
    multiplier: f64,
    colored: bool,
) -> String {
    if selector != Selector::Requests || recommended == RecValue::Unknown {
        return String::new();
    }

    let diff = recommended.number_or_zero() - allocated.number_or_zero();
    let sign = match (diff >= 0.0, colored) {
        (true, false) => "+".to_string(),
        (false, false) => "-".to_string(),
        (true, true) => format!("{}", "+".green()),
        (false, true) => format!("{}", "-".red()),
    };

    format!(
        "{sign}{}",
        format_recommendation_value(RecValue::Val(diff.abs() * multiplier))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Kind, PodData, RecValue::*};

    fn allocations(
        requests: (RecValue, RecValue),
        limits: (RecValue, RecValue),
    ) -> ResourceAllocations {
        ResourceAllocations {
            requests: ByResource {
                cpu: requests.0,
                memory: requests.1,
            },
            limits: ByResource {
                cpu: limits.0,
                memory: limits.1,
            },
            info: ByResource::default(),
        }
    }

    fn object(allocations: ResourceAllocations) -> K8sObject {
        K8sObject {
            cluster: Some("mock-cluster".into()),
            name: "mock-object-1".into(),
            container: "mock-container-1".into(),
            pods: vec![
                PodData {
                    name: "p1".into(),
                    deleted: false,
                },
                PodData {
                    name: "p2".into(),
                    deleted: false,
                },
                PodData {
                    name: "p3".into(),
                    deleted: true,
                },
            ],
            hpa: None,
            namespace: "default".into(),
            kind: Kind::Deployment,
            allocations,
            warnings: Default::default(),
            labels: None,
            annotations: None,
            pod_selector: None,
            uid: None,
        }
    }

    fn scan_with(severity: Severity) -> ResourceScan {
        let recommendation = Recommendation {
            value: Val(1.0),
            severity,
        };
        ResourceScan {
            object: object(allocations((Unset, Unset), (Unset, Unset))),
            recommended: ResourceRecommendation {
                requests: ByResource {
                    cpu: recommendation.clone(),
                    memory: recommendation.clone(),
                },
                limits: ByResource {
                    cpu: recommendation.clone(),
                    memory: recommendation,
                },
                info: ByResource::default(),
            },
            severity,
        }
    }

    #[test]
    fn diff_is_only_rendered_for_requests() {
        assert_eq!(
            format_diff(Val(0.05), Val(0.0065), Selector::Limits, 1.0, false),
            ""
        );
        assert_eq!(
            format_diff(Val(0.05), Unknown, Selector::Requests, 1.0, false),
            ""
        );
    }

    #[test]
    fn diff_matches_upstream_golden_values() {
        // cpu: 50m allocated, 6.5m recommended, over two running pods
        assert_eq!(
            format_diff(Val(0.05), Val(0.0065), Selector::Requests, 1.0, false),
            "-43m"
        );
        assert_eq!(
            format_diff(Val(0.05), Val(0.0065), Selector::Requests, 2.0, false),
            "-87m"
        );

        // memory: 2048Mi allocated, 500m recommended
        assert_eq!(
            format_diff(Val(2147483648.0), Val(0.5), Selector::Requests, 1.0, false),
            "-2048Mi"
        );
        assert_eq!(
            format_diff(Val(2147483648.0), Val(0.5), Selector::Requests, 2.0, false),
            "-4096Mi"
        );
    }

    #[test]
    fn diff_of_an_increase_is_signed_plus() {
        assert_eq!(
            format_diff(Val(0.05), Val(0.1), Selector::Requests, 1.0, false),
            "+50m"
        );
        assert_eq!(
            format_diff(Unset, Val(0.1), Selector::Requests, 1.0, false),
            "+100m"
        );
    }

    #[test]
    fn colored_diff_wraps_the_sign_only() {
        let diff = format_diff(Val(0.05), Val(0.1), Selector::Requests, 1.0, true);
        assert!(diff.contains("50m"), "{diff}");
        assert!(diff.contains('\u{1b}'), "expected ansi colors in {diff:?}");
    }

    #[test]
    fn scan_severity_is_the_worst_of_its_recommendations() {
        let current = allocations((Val(1.0), Val(0.0)), (Unset, Unset));
        // cpu request differs by 0.3 (WARNING), memory by 50Mi (GOOD)
        let recommended = allocations((Val(1.3), Val(50.0 * 1024.0 * 1024.0)), (Unset, Unset));

        let scan = ResourceScan::calculate(object(current), recommended);

        assert_eq!(scan.severity, Severity::Warning);
        assert_eq!(
            scan.recommended.requests[ResourceType::Cpu].severity,
            Severity::Warning
        );
        assert_eq!(
            scan.recommended.requests[ResourceType::Memory].severity,
            Severity::Good
        );
        assert_eq!(scan.recommended.limits[ResourceType::Cpu].value, Unset);
        assert_eq!(
            scan.recommended.get(Selector::Requests)[ResourceType::Cpu].value,
            Val(1.3)
        );
    }

    #[test]
    fn recommendation_info_is_carried_over() {
        let mut recommended = allocations((Val(1.0), Val(1.0)), (Unset, Unset));
        recommended.info.cpu = Some("Not enough data".into());

        let scan = ResourceScan::calculate(
            object(allocations((Val(1.0), Val(1.0)), (Unset, Unset))),
            recommended,
        );

        assert_eq!(
            scan.recommended.info[ResourceType::Cpu].as_deref(),
            Some("Not enough data")
        );
        assert_eq!(scan.recommended.info[ResourceType::Memory], None);
    }

    #[test]
    fn score_counts_critical_and_warning_scans() {
        assert_eq!(ScanResult::calculate_score(&[]), 0);
        assert_eq!(
            ScanResult::calculate_score(&[scan_with(Severity::Good)]),
            100
        );
        assert_eq!(
            ScanResult::calculate_score(&[scan_with(Severity::Critical)]),
            0
        );

        // 0.7 penalty for a warning: (2 - 0.7) / 2 * 100 = 65
        let scans = vec![scan_with(Severity::Good), scan_with(Severity::Warning)];
        assert_eq!(ScanResult::calculate_score(&scans), 65);
    }

    #[test]
    fn score_letters_follow_upstream_bands() {
        let letter = |score: i64| {
            let mut result = ScanResult::new(
                vec![],
                StrategyData {
                    name: "simple".into(),
                    settings: serde_json::json!({}),
                },
                Config::default(),
            );
            result.score = score;
            result.score_letter()
        };

        assert_eq!(letter(29), "F");
        assert_eq!(letter(30), "D");
        assert_eq!(letter(55), "C");
        assert_eq!(letter(70), "B");
        assert_eq!(letter(90), "A");
    }

    #[test]
    fn new_result_computes_score_and_defaults() {
        let scans = vec![scan_with(Severity::Critical), scan_with(Severity::Good)];
        let result = ScanResult::new(
            scans,
            StrategyData {
                name: "simple".into(),
                settings: serde_json::json!({"cpu_percentile": 95.0}),
            },
            Config::default(),
        );

        assert_eq!(result.score, 50);
        assert_eq!(result.resources, vec!["cpu", "memory"]);
        assert!(result.config.is_some());
    }
}
