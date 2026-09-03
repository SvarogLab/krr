//! Loading workload history from a metrics store.
//!
//! Port of `robusta_krr/core/integrations/prometheus/metrics_service/*` and `loader.py`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Context, Result};
use futures::future::join_all;

use crate::model::{K8sObject, Kind, PodData};

use super::client::{PrometheusClient, Series, filter_duplicate_jobs};
use super::metrics::{Metric, QueryType, duration_literal, step_literal};
use super::{MetricsData, PodSeries};

/// Maximum number of pods per query; longer pod lists are split.
const PODS_BATCH_SIZE: usize = 50;
/// Maximum number of owners or pods per lookup query.
const OWNER_BATCH_SIZE: usize = 100;

pub struct MetricsLoader {
    pub client: PrometheusClient,
    /// PromQL fragment selecting the cluster in a centralized Prometheus.
    pub cluster_selector: String,
}

impl MetricsLoader {
    pub fn new(client: PrometheusClient, cluster_selector: String) -> Self {
        MetricsLoader {
            client,
            cluster_selector,
        }
    }

    /// Loads every metric a strategy asked for. Metrics that fail are left out.
    pub async fn gather(
        &self,
        object: &K8sObject,
        metrics: &[Metric],
        period: Duration,
        step: Duration,
    ) -> MetricsData {
        let loaded = join_all(metrics.iter().map(|metric| async move {
            (
                *metric,
                self.load_metric(object, *metric, period, step).await,
            )
        }))
        .await;

        let mut data = MetricsData::new();
        for (metric, result) in loaded {
            match result {
                Ok(series) if series.is_empty() => {
                    if metric.warn_on_no_data() {
                        tracing::warn!(
                            "{} returned no {} metrics for {object}",
                            self.client.name,
                            metric.key()
                        );
                    }
                }
                Ok(series) => {
                    data.insert(metric.key().to_string(), series);
                }
                Err(error) => {
                    tracing::error!("failed to gather {} for {object}: {error:#}", metric.key());
                }
            }
        }
        data
    }

    /// Loads one metric, splitting workloads with many pods into several queries.
    pub async fn load_metric(
        &self,
        object: &K8sObject,
        metric: Metric,
        period: Duration,
        step: Duration,
    ) -> Result<PodSeries> {
        if object.pods.len() > PODS_BATCH_SIZE {
            let batches = object.split_into_batches(PODS_BATCH_SIZE);
            let results = join_all(
                batches
                    .iter()
                    .map(|batch| self.load_metric(batch, metric, period, step)),
            )
            .await;

            let mut combined = PodSeries::new();
            for result in results {
                combined.extend(result?);
            }
            return Ok(combined);
        }

        let step_str = step_literal(step);
        let query = metric.query(
            object,
            &duration_literal(period),
            &step_str,
            &self.cluster_selector,
        );

        let series = match metric.query_type() {
            QueryType::Instant => self.client.query(&query).await?,
            QueryType::Range => {
                let end = now_seconds();
                self.client
                    .query_range(&query, end - period.as_secs_f64(), end, step)
                    .await?
            }
        };

        Ok(series_by_pod(filter_duplicate_jobs(series)))
    }

    /// Pods that belonged to the workload during the period, current and deleted ones.
    ///
    /// Uses kube-state-metrics, so pods that no longer exist are still found.
    pub async fn load_pods(&self, object: &K8sObject, period: Duration) -> Result<Vec<PodData>> {
        let window = owner_period_literal(period);
        let cluster = &self.cluster_selector;
        let namespace = &object.namespace;

        // Deployments and the like own their pods through an intermediate resource.
        let (owners, owner_kind) = match object.kind {
            Kind::Deployment | Kind::Rollout => (
                self.owners_of(object, &window, "kube_replicaset_owner", "replicaset")
                    .await?,
                "ReplicaSet",
            ),
            Kind::DeploymentConfig => (
                self.owners_of(
                    object,
                    &window,
                    "kube_replicationcontroller_owner",
                    "replicationcontroller",
                )
                .await?,
                "ReplicationController",
            ),
            Kind::CronJob => (
                self.owners_of(object, &window, "kube_job_owner", "job_name")
                    .await?,
                "Job",
            ),
            // a group stands for the jobs it collected
            Kind::GroupedJob if !object.grouped_jobs.is_empty() => {
                (object.grouped_jobs.clone(), "Job")
            }
            _ => (vec![object.name.clone()], object.kind.as_str()),
        };

        let mut related_pods: Vec<String> = Vec::new();
        for batch in owners.chunks(OWNER_BATCH_SIZE) {
            let query = format!(
                "last_over_time(kube_pod_owner{{owner_name=~\"{}\", owner_kind=\"{owner_kind}\", namespace=\"{namespace}\"{cluster}}}[{window}])",
                batch.join("|")
            );
            related_pods.extend(label_values(self.client.query(&query).await?, "pod"));
        }

        if related_pods.is_empty() {
            return Ok(Vec::new());
        }

        let mut running: BTreeSet<String> = BTreeSet::new();
        for batch in related_pods.chunks(OWNER_BATCH_SIZE) {
            let query = format!(
                "kube_pod_status_phase{{phase=\"Running\", pod=~\"{}\", namespace=\"{namespace}\"{cluster}}} == 1",
                batch.join("|")
            );
            running.extend(label_values(self.client.query(&query).await?, "pod"));
        }

        let unique: BTreeSet<String> = related_pods.into_iter().collect();
        Ok(unique
            .into_iter()
            .map(|pod| PodData {
                deleted: !running.contains(&pod),
                name: pod,
            })
            .collect())
    }

    /// Names of the resources that own the pods of this workload.
    async fn owners_of(
        &self,
        object: &K8sObject,
        window: &str,
        metric: &str,
        label: &str,
    ) -> Result<Vec<String>> {
        let query = format!(
            "{metric}{{owner_name=\"{}\", owner_kind=\"{}\", namespace=\"{}\"{}}}[{window}]",
            object.name, object.kind, object.namespace, self.cluster_selector
        );
        Ok(label_values(self.client.query(&query).await?, label))
    }

    /// How much history the store actually holds, as `(first, last)` unix timestamps.
    pub async fn history_range(&self, period: Duration) -> Result<(f64, f64)> {
        let end = now_seconds();
        let series = self
            .client
            .query_range(
                "max(prometheus_tsdb_head_series)",
                end - period.as_secs_f64(),
                end,
                Duration::from_secs(3600),
            )
            .await?;

        let values = series
            .first()
            .map(|series| &series.values)
            .filter(|values| !values.is_empty());
        let values = values.context("prometheus returned no history range")?;
        Ok((values[0].0, values[values.len() - 1].0))
    }

    /// Cluster capacity and kube-system requests, reported alongside the scan.
    pub async fn cluster_summary(&self) -> BTreeMap<String, serde_json::Value> {
        let cluster = &self.cluster_selector;
        // queries without other labels need the fragment without its leading comma
        let bare = cluster.trim_start_matches(',').trim();

        let queries = [
            (
                "cluster_memory",
                format!("sum(max by (instance) (machine_memory_bytes{{{bare}}}))"),
            ),
            (
                "cluster_cpu",
                format!("sum(max by (instance) (machine_cpu_cores{{{bare}}}))"),
            ),
            (
                "kube_system_mem_req",
                format!(
                    "sum(max(kube_pod_container_resource_requests{{namespace=\"kube-system\", resource=\"memory\"{cluster}}}) by (job, pod, container))"
                ),
            ),
            (
                "kube_system_cpu_req",
                format!(
                    "sum(max(kube_pod_container_resource_requests{{namespace=\"kube-system\", resource=\"cpu\"{cluster}}}) by (job, pod, container))"
                ),
            ),
        ];

        let results = join_all(
            queries
                .iter()
                .map(|(name, query)| async move { (*name, self.single_value(query).await) }),
        )
        .await;

        let mut summary = BTreeMap::new();
        for (name, value) in results {
            match value {
                Ok(value) => {
                    summary.insert(name.to_string(), serde_json::json!(value));
                }
                Err(error) => {
                    tracing::warn!("could not read {name} from {}: {error:#}", self.client.name);
                    return BTreeMap::new();
                }
            }
        }
        summary
    }

    /// Runs a query that is expected to return exactly one value.
    async fn single_value(&self, query: &str) -> Result<f64> {
        let series = self.client.query(query).await?;
        if series.len() != 1 {
            anyhow::bail!("expected exactly one result, got {}: {query}", series.len());
        }
        let value = series[0]
            .values
            .first()
            .context("result carries no value")?;
        Ok(value.1)
    }
}

/// Seconds since the epoch, truncated to a whole minute like upstream does.
fn now_seconds() -> f64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is after 1970")
        .as_secs();
    (now - now % 60) as f64
}

/// Collects one label from every series, keeping duplicates out.
fn label_values(series: Vec<Series>, label: &str) -> Vec<String> {
    let mut values: Vec<String> = series
        .into_iter()
        .filter_map(|s| s.metric.get(label).cloned())
        .collect();
    values.sort();
    values.dedup();
    values
}

/// Groups the series of one query by pod name.
fn series_by_pod(series: Vec<Series>) -> PodSeries {
    series
        .into_iter()
        .filter_map(|series| {
            series
                .metric
                .get("pod")
                .cloned()
                .map(|pod| (pod, series.values))
        })
        .collect()
}

/// The `[<period>]` literal used when looking up pod owners.
fn owner_period_literal(period: Duration) -> String {
    let seconds = period.as_secs();
    if seconds <= 24 * 3600 {
        format!("{}h", (seconds / 3600).min(32))
    } else {
        format!("{}d", (seconds / 3600 / 24).min(32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ResourceAllocations;
    use wiremock::matchers::{method, query_param_contains};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn object(kind: Kind, pods: &[&str]) -> K8sObject {
        K8sObject {
            cluster: None,
            name: "web".into(),
            container: "app".into(),
            pods: pods
                .iter()
                .map(|name| PodData {
                    name: name.to_string(),
                    deleted: false,
                })
                .collect(),
            hpa: None,
            namespace: "default".into(),
            kind,
            allocations: ResourceAllocations::default(),
            warnings: Default::default(),
            labels: None,
            annotations: None,
            pod_selector: None,
            uid: None,
            grouped_jobs: Vec::new(),
        }
    }

    fn loader(server: &MockServer) -> MetricsLoader {
        let client =
            PrometheusClient::direct("Prometheus", &server.uri(), &BTreeMap::new(), false).unwrap();
        MetricsLoader::new(client, String::new())
    }

    /// A vector response with one series per `(pod, value)`.
    fn vector(pods: &[(&str, f64)]) -> String {
        let series: Vec<String> = pods
            .iter()
            .map(|(pod, value)| {
                format!(r#"{{"metric":{{"pod":"{pod}","job":"kubelet"}},"value":[1700000000,"{value}"]}}"#)
            })
            .collect();
        format!(
            r#"{{"status":"success","data":{{"resultType":"vector","result":[{}]}}}}"#,
            series.join(",")
        )
    }

    #[tokio::test]
    async fn gathers_metrics_by_pod() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(vector(&[("web-1", 0.5), ("web-2", 1.5)])),
            )
            .mount(&server)
            .await;

        let data = loader(&server)
            .gather(
                &object(Kind::Deployment, &["web-1", "web-2"]),
                &[Metric::MaxMemory],
                Duration::from_secs(3600),
                Duration::from_secs(75),
            )
            .await;

        let series = &data[Metric::MaxMemory.key()];
        assert_eq!(series["web-1"], vec![(1700000000.0, 0.5)]);
        assert_eq!(series["web-2"], vec![(1700000000.0, 1.5)]);
    }

    #[tokio::test]
    async fn a_failing_metric_is_left_out_instead_of_failing_the_scan() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let mut loader = loader(&server);
        loader.client.retries = 0;
        let data = loader
            .gather(
                &object(Kind::Deployment, &["web-1"]),
                &[Metric::MaxMemory],
                Duration::from_secs(3600),
                Duration::from_secs(75),
            )
            .await;

        assert!(data.is_empty(), "{data:?}");
    }

    #[tokio::test]
    async fn many_pods_are_queried_in_batches() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(vector(&[("web-1", 1.0)])))
            .mount(&server)
            .await;

        let pods: Vec<String> = (0..120).map(|i| format!("web-{i}")).collect();
        let names: Vec<&str> = pods.iter().map(String::as_str).collect();
        loader(&server)
            .load_metric(
                &object(Kind::Deployment, &names),
                Metric::MaxMemory,
                Duration::from_secs(3600),
                Duration::from_secs(75),
            )
            .await
            .unwrap();

        // 120 pods, 50 per query
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn range_metrics_use_the_range_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(vector(&[("web-1", 1.0)])))
            .mount(&server)
            .await;

        assert_eq!(Metric::Cpu.query_type(), QueryType::Range);
        loader(&server)
            .load_metric(
                &object(Kind::Deployment, &["web-1"]),
                Metric::Cpu,
                Duration::from_secs(3600),
                Duration::from_secs(75),
            )
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert!(
            requests[0].url.path().ends_with("/query_range"),
            "{}",
            requests[0].url
        );
    }

    #[tokio::test]
    async fn loads_pods_of_a_deployment_through_its_replicasets() {
        let server = MockServer::start().await;

        let replicasets = r#"{"status":"success","data":{"resultType":"vector","result":[
            {"metric":{"replicaset":"web-abc"},"value":[1,"1"]}]}}"#;
        let pod_owners = r#"{"status":"success","data":{"resultType":"vector","result":[
            {"metric":{"pod":"web-abc-1"},"value":[1,"1"]},
            {"metric":{"pod":"web-abc-2"},"value":[1,"1"]}]}}"#;
        let running = r#"{"status":"success","data":{"resultType":"vector","result":[
            {"metric":{"pod":"web-abc-1"},"value":[1,"1"]}]}}"#;

        Mock::given(method("GET"))
            .and(query_param_contains("query", "kube_replicaset_owner"))
            .respond_with(ResponseTemplate::new(200).set_body_string(replicasets))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param_contains("query", "kube_pod_owner"))
            .respond_with(ResponseTemplate::new(200).set_body_string(pod_owners))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param_contains("query", "kube_pod_status_phase"))
            .respond_with(ResponseTemplate::new(200).set_body_string(running))
            .mount(&server)
            .await;

        let mut pods = loader(&server)
            .load_pods(&object(Kind::Deployment, &[]), Duration::from_secs(3600))
            .await
            .unwrap();
        pods.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(
            pods,
            vec![
                PodData {
                    name: "web-abc-1".into(),
                    deleted: false
                },
                PodData {
                    name: "web-abc-2".into(),
                    deleted: true
                },
            ]
        );
    }

    #[tokio::test]
    async fn workloads_without_an_intermediate_owner_are_queried_directly() {
        let server = MockServer::start().await;
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let recorder = seen.clone();

        Mock::given(method("GET"))
            .respond_with(move |request: &Request| {
                let query = request
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "query")
                    .unwrap()
                    .1
                    .to_string();
                recorder.lock().unwrap().push(query);
                ResponseTemplate::new(200).set_body_string(
                    r#"{"status":"success","data":{"resultType":"vector","result":[]}}"#,
                )
            })
            .mount(&server)
            .await;

        loader(&server)
            .load_pods(&object(Kind::StatefulSet, &[]), Duration::from_secs(3600))
            .await
            .unwrap();

        let queries = seen.lock().unwrap().clone();
        assert!(
            queries
                .iter()
                .all(|query| !query.contains("kube_replicaset_owner")),
            "{queries:?}"
        );
        assert!(
            queries[0].contains(r#"owner_kind="StatefulSet""#),
            "{queries:?}"
        );
        assert!(queries[0].contains(r#"owner_name=~"web""#), "{queries:?}");
    }

    #[tokio::test]
    async fn grouped_jobs_are_queried_by_their_job_names() {
        let server = MockServer::start().await;
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        let recorder = seen.clone();

        Mock::given(method("GET"))
            .respond_with(move |request: &Request| {
                let query = request
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "query")
                    .unwrap()
                    .1
                    .to_string();
                recorder.lock().unwrap().push(query);
                ResponseTemplate::new(200).set_body_string(
                    r#"{"status":"success","data":{"resultType":"vector","result":[]}}"#,
                )
            })
            .mount(&server)
            .await;

        let mut group = object(Kind::GroupedJob, &[]);
        group.name = "app=etl".into();
        group.grouped_jobs = vec!["batch-1".into(), "batch-2".into()];

        loader(&server)
            .load_pods(&group, Duration::from_secs(3600))
            .await
            .unwrap();

        let queries = seen.lock().unwrap().clone();
        assert!(queries[0].contains(r#"owner_kind="Job""#), "{queries:?}");
        assert!(
            queries[0].contains(r#"owner_name=~"batch-1|batch-2""#),
            "{queries:?}"
        );
    }

    #[test]
    fn owner_period_is_capped_at_32_units() {
        assert_eq!(owner_period_literal(Duration::from_secs(3 * 3600)), "3h");
        assert_eq!(owner_period_literal(Duration::from_secs(24 * 3600)), "24h");
        assert_eq!(
            owner_period_literal(Duration::from_secs(14 * 24 * 3600)),
            "14d"
        );
        assert_eq!(
            owner_period_literal(Duration::from_secs(90 * 24 * 3600)),
            "32d"
        );
    }

    #[tokio::test]
    async fn reads_the_available_history_range() {
        let server = MockServer::start().await;
        let body = r#"{"status":"success","data":{"resultType":"matrix","result":[
            {"metric":{},"values":[[1700000000,"1"],[1700003600,"2"]]}]}}"#;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let (start, end) = loader(&server)
            .history_range(Duration::from_secs(5 * 3600))
            .await
            .unwrap();
        assert_eq!((start, end), (1700000000.0, 1700003600.0));
    }

    #[tokio::test]
    async fn an_empty_history_range_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"status":"success","data":{"resultType":"matrix","result":[]}}"#,
            ))
            .mount(&server)
            .await;

        assert!(
            loader(&server)
                .history_range(Duration::from_secs(3600))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn summarizes_the_cluster() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(vector(&[("", 8.0)])))
            .mount(&server)
            .await;

        let summary = loader(&server).cluster_summary().await;
        assert_eq!(summary["cluster_cpu"], serde_json::json!(8.0));
        assert_eq!(summary["cluster_memory"], serde_json::json!(8.0));
        assert_eq!(summary["kube_system_cpu_req"], serde_json::json!(8.0));
        assert_eq!(summary["kube_system_mem_req"], serde_json::json!(8.0));
    }

    #[tokio::test]
    async fn a_failing_summary_is_empty_rather_than_fatal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let mut loader = loader(&server);
        loader.client.retries = 0;
        assert!(loader.cluster_summary().await.is_empty());
    }
}
