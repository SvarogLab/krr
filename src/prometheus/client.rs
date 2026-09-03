//! A thin Prometheus HTTP API client.
//!
//! Replaces `prometrix`/`prometheus_api_client` upstream: KRR only needs
//! `/api/v1/query`, `/api/v1/query_range` and `/api/v1/label/<name>/values`.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::Deserialize;

/// Everything but the unreserved characters of RFC 3986 is escaped in query values.
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// One time series with its labels.
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub metric: BTreeMap<String, String>,
    /// `(timestamp, value)` pairs; instant queries carry exactly one.
    pub values: Vec<(f64, f64)>,
}

impl Series {
    /// The label identifying what the series is about, as upstream picks it.
    pub fn target_name(&self) -> Option<&str> {
        ["pod", "container", "node"]
            .iter()
            .find_map(|label| self.metric.get(*label))
            .map(String::as_str)
    }
}

/// How requests reach Prometheus.
pub enum Transport {
    /// Straight HTTP(S), optionally with extra headers.
    Direct {
        client: reqwest::Client,
        base_url: String,
    },
    /// Through the Kubernetes API server's service proxy, reusing its credentials.
    KubeProxy {
        client: kube::Client,
        base_path: String,
    },
}

pub struct PrometheusClient {
    transport: Transport,
    /// Name of the metrics store, used in log messages.
    pub name: String,
    /// How often a failed request is retried.
    pub retries: u32,
}

impl PrometheusClient {
    /// A client talking to `url` directly.
    pub fn direct(
        name: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        insecure_tls: bool,
    ) -> Result<Self> {
        let mut header_map = reqwest::header::HeaderMap::new();
        for (key, value) in headers {
            header_map.insert(
                reqwest::header::HeaderName::from_bytes(key.as_bytes())
                    .with_context(|| format!("invalid header name: {key}"))?,
                reqwest::header::HeaderValue::from_str(value)
                    .with_context(|| format!("invalid value for header {key}"))?,
            );
        }

        let client = reqwest::Client::builder()
            .default_headers(header_map)
            .danger_accept_invalid_certs(insecure_tls)
            .timeout(Duration::from_secs(120))
            .build()?;

        Ok(PrometheusClient {
            transport: Transport::Direct {
                client,
                base_url: url.trim_end_matches('/').to_string(),
            },
            name: name.to_string(),
            retries: 4,
        })
    }

    /// A client reaching a service through the Kubernetes API server proxy.
    pub fn kube_proxy(name: &str, client: kube::Client, base_path: &str) -> Self {
        PrometheusClient {
            transport: Transport::KubeProxy {
                client,
                base_path: base_path.trim_end_matches('/').to_string(),
            },
            name: name.to_string(),
            retries: 4,
        }
    }

    /// `/api/v1/query`: one value per series.
    pub async fn query(&self, query: &str) -> Result<Vec<Series>> {
        let body = self
            .get("/api/v1/query", &[("query", query.to_string())])
            .await?;
        parse_series(&body).with_context(|| format!("failed to run query: {query}"))
    }

    /// `/api/v1/query_range`: a series of values per series.
    pub async fn query_range(
        &self,
        query: &str,
        start: f64,
        end: f64,
        step: Duration,
    ) -> Result<Vec<Series>> {
        let params = [
            ("query", query.to_string()),
            ("start", format!("{}", start.round() as i64)),
            ("end", format!("{}", end.round() as i64)),
            ("step", format!("{}s", step.as_secs())),
        ];
        let body = self.get("/api/v1/query_range", &params).await?;
        parse_series(&body).with_context(|| format!("failed to run query: {query}"))
    }

    /// Values a label takes, used to list the clusters of a centralized Prometheus.
    pub async fn label_values(&self, label: &str) -> Result<Vec<String>> {
        let body = self
            .get(&format!("/api/v1/label/{label}/values"), &[])
            .await?;
        parse_label_values(&body)
    }

    /// Fails when the store is not reachable or does not answer queries.
    pub async fn check_connection(&self) -> Result<()> {
        self.query("up")
            .await
            .with_context(|| format!("could not connect to {}", self.name))?;
        Ok(())
    }

    /// Runs a request, retrying the failures that can pass.
    async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<String> {
        let mut attempt = 0;
        loop {
            let failure = match self.get_once(path, params).await {
                Ok(body) => return Ok(body),
                Err(failure) => failure,
            };

            // a rejected query fails the same way however often it is sent
            if !failure.retryable || attempt == self.retries {
                return Err(failure.error);
            }

            tracing::debug!(
                "{} request failed (attempt {}): {:#}",
                self.name,
                attempt + 1,
                failure.error
            );
            // upstream waits a random 2-10s; a short exponential backoff serves the same purpose
            tokio::time::sleep(Duration::from_millis(200 * 2u64.pow(attempt.min(5)))).await;
            attempt += 1;
        }
    }

    async fn get_once(&self, path: &str, params: &[(&str, String)]) -> Result<String, Failure> {
        match &self.transport {
            Transport::Direct { client, base_url } => {
                let response = client
                    .get(format!("{base_url}{path}"))
                    .query(params)
                    .send()
                    .await
                    .map_err(|error| Failure::retryable(error.into()))?;

                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                if !status.is_success() {
                    let error = anyhow::anyhow!(
                        "{} returned {}: {}",
                        self.name,
                        status.as_u16(),
                        body.trim()
                    );
                    return Err(Failure {
                        error,
                        retryable: is_retryable(status.as_u16()),
                    });
                }
                Ok(body)
            }
            Transport::KubeProxy { client, base_path } => {
                let query = params
                    .iter()
                    .map(|(key, value)| {
                        format!("{key}={}", utf8_percent_encode(value, QUERY_VALUE))
                    })
                    .collect::<Vec<_>>()
                    .join("&");
                let uri = if query.is_empty() {
                    format!("{base_path}{path}")
                } else {
                    format!("{base_path}{path}?{query}")
                };

                let request = http::Request::get(uri)
                    .body(Vec::new())
                    .map_err(|error| Failure::permanent(error.into()))?;

                client.request_text(request).await.map_err(|error| {
                    // the proxied body is not a kubernetes Status, so kube cannot read it
                    let kube::Error::Api(status) = &error else {
                        return Failure::retryable(error.into());
                    };
                    let detail = prometheus_error(&status.message)
                        .unwrap_or_else(|| status.message.trim().to_string());
                    Failure {
                        error: anyhow::anyhow!("{} returned {}: {detail}", self.name, status.code),
                        retryable: is_retryable(status.code),
                    }
                })
            }
        }
    }
}

/// A failed request, and whether sending it again could help.
struct Failure {
    error: anyhow::Error,
    retryable: bool,
}

impl Failure {
    fn retryable(error: anyhow::Error) -> Self {
        Failure {
            error,
            retryable: true,
        }
    }

    fn permanent(error: anyhow::Error) -> Self {
        Failure {
            error,
            retryable: false,
        }
    }
}

/// Reads the human part out of a Prometheus API error body.
fn prometheus_error(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?.as_str()?;
    match value.get("errorType").and_then(|kind| kind.as_str()) {
        Some(kind) => Some(format!("{kind}: {error}")),
        None => Some(error.to_string()),
    }
}

/// Server errors and rate limits can pass; a rejected request cannot.
fn is_retryable(status: u16) -> bool {
    status == 429 || !(400..500).contains(&status)
}

// ------------------------------- response parsing -------------------------------

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    status: String,
    data: Option<T>,
    error: Option<String>,
    #[serde(rename = "errorType")]
    error_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct QueryData {
    result: Vec<RawSeries>,
}

#[derive(Debug, Deserialize)]
struct RawSeries {
    #[serde(default)]
    metric: BTreeMap<String, String>,
    /// Instant queries.
    #[serde(default)]
    value: Option<(f64, String)>,
    /// Range queries.
    #[serde(default)]
    values: Vec<(f64, String)>,
}

impl<T> ApiResponse<T> {
    /// Unwraps a response, turning an `"error"` status into an error.
    fn into_data(self) -> Result<T> {
        if self.status != "success" {
            let kind = self.error_type.unwrap_or_else(|| "error".to_string());
            let message = self.error.unwrap_or_else(|| "unknown error".to_string());
            bail!("prometheus returned {kind}: {message}");
        }
        self.data.context("prometheus response has no data")
    }
}

/// Parses a query or query_range response body into series.
pub fn parse_series(body: &str) -> Result<Vec<Series>> {
    let response: ApiResponse<QueryData> =
        serde_json::from_str(body).context("could not parse the prometheus response")?;

    let parse_point = |(timestamp, value): &(f64, String)| -> Result<(f64, f64)> {
        let value = value
            .parse::<f64>()
            .with_context(|| format!("could not parse the sample value {value:?}"))?;
        Ok((*timestamp, value))
    };

    response
        .into_data()?
        .result
        .into_iter()
        .map(|raw| {
            let points = raw.value.iter().chain(raw.values.iter());
            Ok(Series {
                metric: raw.metric,
                values: points.map(parse_point).collect::<Result<Vec<_>>>()?,
            })
        })
        .collect()
}

/// Parses a `label/<name>/values` response body.
pub fn parse_label_values(body: &str) -> Result<Vec<String>> {
    let response: ApiResponse<Vec<String>> =
        serde_json::from_str(body).context("could not parse the prometheus response")?;
    response.into_data()
}

/// Keeps one series per target, preferring the `kubelet` job like upstream does.
pub fn filter_duplicate_jobs(series: Vec<Series>) -> Vec<Series> {
    if series.len() <= 1 {
        return series;
    }

    let mut targets: Vec<&str> = series.iter().filter_map(Series::target_name).collect();
    targets.sort_unstable();
    targets.dedup();

    let job_of = |series: &Series| series.metric.get("job").cloned().unwrap_or_default();

    targets
        .into_iter()
        .filter_map(|target| {
            let mut candidates: Vec<&Series> = series
                .iter()
                .filter(|series| series.target_name() == Some(target))
                .collect();
            candidates.sort_by_key(|series| job_of(series));

            let kubelet: Vec<&&Series> = candidates
                .iter()
                .filter(|series| job_of(series) == "kubelet")
                .collect();
            if kubelet.len() == 1 {
                return Some((*kubelet[0]).clone());
            }
            candidates.first().map(|series| (*series).clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn series(labels: &[(&str, &str)], values: &[(f64, f64)]) -> Series {
        Series {
            metric: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            values: values.to_vec(),
        }
    }

    async fn client(server: &MockServer) -> PrometheusClient {
        PrometheusClient::direct("Prometheus", &server.uri(), &BTreeMap::new(), false).unwrap()
    }

    const VECTOR: &str = r#"{
        "status": "success",
        "data": {
            "resultType": "vector",
            "result": [
                {"metric": {"pod": "web-1", "job": "kubelet"}, "value": [1700000000, "0.25"]}
            ]
        }
    }"#;

    const MATRIX: &str = r#"{
        "status": "success",
        "data": {
            "resultType": "matrix",
            "result": [
                {"metric": {"pod": "web-1"}, "values": [[1700000000, "0.1"], [1700000060, "0.2"]]}
            ]
        }
    }"#;

    #[test]
    fn query_values_keep_only_unreserved_characters() {
        let query = r#"max(x{pod=~"a|b"}) by (pod)"#;
        let encoded = utf8_percent_encode(query, QUERY_VALUE).to_string();
        assert_eq!(
            encoded,
            "max%28x%7Bpod%3D~%22a%7Cb%22%7D%29%20by%20%28pod%29"
        );
        assert_eq!(
            utf8_percent_encode("a-b_c.d~e", QUERY_VALUE).to_string(),
            "a-b_c.d~e"
        );
    }

    #[test]
    fn parses_instant_values_as_a_single_point() {
        let parsed = parse_series(VECTOR).unwrap();
        assert_eq!(
            parsed,
            vec![series(
                &[("pod", "web-1"), ("job", "kubelet")],
                &[(1700000000.0, 0.25)]
            )]
        );
    }

    #[test]
    fn parses_range_values() {
        let parsed = parse_series(MATRIX).unwrap();
        assert_eq!(
            parsed[0].values,
            vec![(1700000000.0, 0.1), (1700000060.0, 0.2)]
        );
    }

    #[test]
    fn parses_special_float_values() {
        let body = r#"{"status":"success","data":{"resultType":"vector","result":[
            {"metric":{},"value":[1.0,"NaN"]}]}}"#;
        assert!(parse_series(body).unwrap()[0].values[0].1.is_nan());
    }

    #[test]
    fn reports_api_errors() {
        let body = r#"{"status":"error","errorType":"bad_data","error":"parse error"}"#;
        let error = parse_series(body).unwrap_err().to_string();
        assert!(error.contains("parse error"), "{error}");
        assert!(error.contains("bad_data"), "{error}");
    }

    #[test]
    fn prometheus_errors_are_unwrapped_from_the_body() {
        let body =
            r#"{"status":"error","errorType":"bad_data","error":"exceeded maximum resolution"}"#;
        assert_eq!(
            prometheus_error(body).unwrap(),
            "bad_data: exceeded maximum resolution"
        );
        assert_eq!(prometheus_error("not json"), None);
    }

    #[test]
    fn parses_label_values() {
        let body = r#"{"status":"success","data":["prod","staging"]}"#;
        assert_eq!(parse_label_values(body).unwrap(), vec!["prod", "staging"]);
    }

    #[test]
    fn target_name_prefers_pod_then_container_then_node() {
        assert_eq!(
            series(&[("pod", "p"), ("container", "c")], &[]).target_name(),
            Some("p")
        );
        assert_eq!(
            series(&[("container", "c"), ("node", "n")], &[]).target_name(),
            Some("c")
        );
        assert_eq!(series(&[("node", "n")], &[]).target_name(), Some("n"));
        assert_eq!(series(&[("job", "j")], &[]).target_name(), None);
    }

    #[test]
    fn duplicate_targets_prefer_the_kubelet_job() {
        let input = vec![
            series(&[("pod", "web-1"), ("job", "cadvisor")], &[(1.0, 1.0)]),
            series(&[("pod", "web-1"), ("job", "kubelet")], &[(1.0, 2.0)]),
            series(&[("pod", "web-2"), ("job", "zzz")], &[(1.0, 3.0)]),
            series(&[("pod", "web-2"), ("job", "aaa")], &[(1.0, 4.0)]),
        ];

        let filtered = filter_duplicate_jobs(input);
        assert_eq!(filtered.len(), 2);

        let by_pod: BTreeMap<&str, &Series> = filtered
            .iter()
            .map(|s| (s.metric["pod"].as_str(), s))
            .collect();
        assert_eq!(by_pod["web-1"].metric["job"], "kubelet");
        // no kubelet job: the first one alphabetically wins
        assert_eq!(by_pod["web-2"].metric["job"], "aaa");
    }

    #[test]
    fn a_single_series_is_never_filtered() {
        let input = vec![series(&[("job", "custom")], &[(1.0, 1.0)])];
        assert_eq!(filter_duplicate_jobs(input.clone()), input);
    }

    #[tokio::test]
    async fn queries_an_instant_value() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/query"))
            .and(query_param("query", "up"))
            .respond_with(ResponseTemplate::new(200).set_body_string(VECTOR))
            .mount(&server)
            .await;

        let result = client(&server).await.query("up").await.unwrap();
        assert_eq!(result[0].values, vec![(1700000000.0, 0.25)]);
    }

    #[tokio::test]
    async fn queries_a_range() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/query_range"))
            .and(query_param("start", "1700000000"))
            .and(query_param("end", "1700000600"))
            .and(query_param("step", "60s"))
            .respond_with(ResponseTemplate::new(200).set_body_string(MATRIX))
            .mount(&server)
            .await;

        let result = client(&server)
            .await
            .query_range("up", 1700000000.0, 1700000600.0, Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(result[0].values.len(), 2);
    }

    #[tokio::test]
    async fn sends_configured_headers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wiremock::matchers::header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_string(VECTOR))
            .mount(&server)
            .await;

        let headers = BTreeMap::from([("authorization".to_string(), "Bearer token".to_string())]);
        let client =
            PrometheusClient::direct("Prometheus", &server.uri(), &headers, false).unwrap();
        assert!(client.query("up").await.is_ok());
    }

    #[tokio::test]
    async fn retries_transient_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(VECTOR))
            .mount(&server)
            .await;

        let result = client(&server).await.query("up").await;
        assert!(result.is_ok(), "{result:?}");
    }

    #[tokio::test]
    async fn gives_up_after_the_last_retry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let mut client = client(&server).await;
        client.retries = 1;
        let error = client.query("up").await.unwrap_err().to_string();
        assert!(error.contains("500"), "{error}");
    }

    #[tokio::test]
    async fn rejected_queries_are_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(400).set_body_string("parse error"))
            .mount(&server)
            .await;

        let error = client(&server)
            .await
            .query("not a query")
            .await
            .unwrap_err();

        assert!(error.to_string().contains("400"), "{error}");
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            1,
            "a 400 means the query itself is wrong, so sending it again cannot help"
        );
    }

    #[tokio::test]
    async fn server_errors_are_retried_to_the_limit() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let mut client = client(&server).await;
        client.retries = 2;
        assert!(client.query("up").await.is_err());
        assert_eq!(server.received_requests().await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn check_connection_reports_unreachable_stores() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(VECTOR))
            .mount(&server)
            .await;
        assert!(client(&server).await.check_connection().await.is_ok());

        let mut unreachable =
            PrometheusClient::direct("Prometheus", "http://127.0.0.1:1", &BTreeMap::new(), false)
                .unwrap();
        unreachable.retries = 0;
        assert!(unreachable.check_connection().await.is_err());
    }
}
