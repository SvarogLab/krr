//! End-to-end test of the library: metrics in, a formatted report out.

use std::collections::BTreeMap;

use krr::config::Config;
use krr::format;
use krr::model::{
    ByResource, K8sObject, Kind, PodData, RecValue, ResourceAllocations, ResourceType,
};
use krr::prometheus::MetricsData;
use krr::prometheus::metrics::Metric;
use krr::result::{ResourceScan, ScanResult, StrategyData};
use krr::runner::round_value;
use krr::severity::Severity;
use krr::strategy::{self, StrategyArgs};

fn allocations(cpu_request: &str, memory_request: &str) -> ResourceAllocations {
    serde_json::from_value(serde_json::json!({
        "requests": {"cpu": cpu_request, "memory": memory_request},
        "limits": {"cpu": null, "memory": memory_request},
    }))
    .unwrap()
}

fn object(name: &str, allocations: ResourceAllocations) -> K8sObject {
    K8sObject {
        cluster: Some("prod".into()),
        name: name.into(),
        container: "app".into(),
        pods: vec![
            PodData {
                name: format!("{name}-1"),
                deleted: false,
            },
            PodData {
                name: format!("{name}-2"),
                deleted: false,
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

/// Metrics as the loader would return them: `cpu` at the requested percentile,
/// `memory` as the peak per pod, plus the data point counts.
fn metrics(cpu: &[f64], memory: &[f64]) -> MetricsData {
    let series = |values: &[f64]| -> BTreeMap<String, Vec<(f64, f64)>> {
        BTreeMap::from([(
            "web-1".to_string(),
            values
                .iter()
                .enumerate()
                .map(|(i, v)| (1700000000.0 + i as f64, *v))
                .collect(),
        )])
    };

    MetricsData::from([
        (Metric::PercentileCpu(95.0).key().to_string(), series(cpu)),
        (Metric::MaxMemory.key().to_string(), series(memory)),
        (Metric::CpuAmount.key().to_string(), series(&[500.0])),
        (Metric::MemoryAmount.key().to_string(), series(&[500.0])),
    ])
}

/// Runs the strategy, rounds the values and builds the report, like the runner does.
fn report(config: &Config, objects: Vec<(K8sObject, MetricsData)>) -> ScanResult {
    let strategy = strategy::find(&config.strategy, &StrategyArgs::default()).unwrap();

    let scans: Vec<ResourceScan> = objects
        .into_iter()
        .map(|(object, data)| {
            let recommendation = strategy.run(&data, &object);
            let allocations = ResourceAllocations {
                requests: ByResource::from_fn(|r| {
                    round_value(recommendation[r].request, r, config)
                }),
                limits: ByResource::from_fn(|r| round_value(recommendation[r].limit, r, config)),
                info: ByResource::from_fn(|r| recommendation[r].info.clone()),
            };
            ResourceScan::calculate(object, allocations)
        })
        .collect();

    let strategy_data = StrategyData {
        name: strategy.name().into(),
        settings: strategy.settings_json(),
    };
    let mut result = ScanResult::new(scans, strategy_data, config.clone());
    result.description = Some(format!("Simple Strategy\n\n{}", strategy.description()));
    result
}

#[test]
fn an_over_provisioned_deployment_is_recommended_down() {
    let config = Config::default();
    let mib = 1024.0 * 1024.0;

    // asks for 1 cpu and 2Gi, uses 65m and 300MiB
    let object = object("web", allocations("1", "2Gi"));
    let result = report(&config, vec![(object, metrics(&[0.065], &[300.0 * mib]))]);

    let scan = &result.scans[0];
    assert_eq!(
        scan.recommended.requests[ResourceType::Cpu].value,
        RecValue::Val(0.065)
    );
    assert_eq!(
        scan.recommended.limits[ResourceType::Cpu].value,
        RecValue::Unset
    );

    // 300MiB + 15% = 345MiB
    assert_eq!(
        scan.recommended.requests[ResourceType::Memory].value,
        RecValue::Val(345.0 * mib)
    );
    assert_eq!(
        scan.recommended.limits[ResourceType::Memory].value,
        RecValue::Val(345.0 * mib)
    );

    // both resources are far off, so the scan is critical and the score is 0
    assert_eq!(scan.severity, Severity::Critical);
    assert_eq!(result.score, 0);
    assert_eq!(result.score_letter(), "F");
}

#[test]
fn recommendations_never_go_below_the_configured_minimums() {
    let config = Config {
        cpu_min_value: 10,
        memory_min_value: 100,
        ..Default::default()
    };
    let object = object("idle", allocations("10m", "128Mi"));

    let result = report(&config, vec![(object, metrics(&[0.0001], &[1024.0]))]);
    let scan = &result.scans[0];

    assert_eq!(
        scan.recommended.requests[ResourceType::Cpu].value,
        RecValue::Val(0.01)
    );
    assert_eq!(
        scan.recommended.requests[ResourceType::Memory].value,
        RecValue::Val(100.0 * 1024.0 * 1024.0)
    );
    // a well sized workload scores 100
    assert_eq!(result.score, 100);
    assert_eq!(result.score_letter(), "A");
}

#[test]
fn workloads_without_history_are_reported_as_unknown() {
    let config = Config::default();
    let object = object("fresh", allocations("500m", "1Gi"));

    let result = report(&config, vec![(object, MetricsData::new())]);
    let scan = &result.scans[0];

    assert_eq!(
        scan.recommended.requests[ResourceType::Cpu].value,
        RecValue::Unknown
    );
    assert_eq!(
        scan.recommended.info[ResourceType::Cpu].as_deref(),
        Some("No data")
    );
    assert_eq!(scan.severity, Severity::Unknown);
}

#[test]
fn every_formatter_renders_the_report() {
    let config = Config {
        show_cluster_name: true,
        ..Default::default()
    };
    let mib = 1024.0 * 1024.0;

    let result = report(
        &config,
        vec![
            (
                object("web", allocations("1", "2Gi")),
                metrics(&[0.065], &[300.0 * mib]),
            ),
            (
                object("api", allocations("200m", "512Mi")),
                metrics(&[0.15], &[400.0 * mib]),
            ),
        ],
    );

    for name in format::names() {
        let formatter = format::find(name).unwrap();
        let output = (formatter.render)(&result).expect(name);

        assert!(
            output.contains("web"),
            "{name} output is missing the workload: {output}"
        );
        assert!(!output.is_empty(), "{name} produced no output");
    }

    // the csv output is machine readable and carries the numbers
    let csv = (format::find("csv").unwrap().render)(&result).unwrap();
    let mut reader = csv::Reader::from_reader(csv.as_bytes());
    let headers: Vec<String> = reader.headers().unwrap().iter().map(String::from).collect();
    assert_eq!(headers[0], "Cluster");

    let rows: Vec<Vec<String>> = reader
        .records()
        .map(|r| r.unwrap().iter().map(String::from).collect())
        .collect();
    assert_eq!(rows.len(), 2);

    let cpu_requests = headers.iter().position(|h| h == "CPU Requests").unwrap();
    assert_eq!(rows[0][cpu_requests], "(-935m) 1.0 -> 65m");
}

#[test]
fn reports_round_trip_through_json() {
    let result = report(
        &Config::default(),
        vec![(
            object("web", allocations("1", "2Gi")),
            metrics(&[0.065], &[300.0 * 1024.0 * 1024.0]),
        )],
    );

    let json = (format::find("json").unwrap().render)(&result).unwrap();
    let parsed: ScanResult = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, result);
}
