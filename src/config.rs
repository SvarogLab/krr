//! Runtime configuration, shared by the loaders, the runner and the formatters.
//!
//! Port of `robusta_krr/core/models/config.py`.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::model::Kind;

/// A `"*" | [values]` filter, as used for namespaces, resources and clusters.
#[derive(Debug, Clone, PartialEq)]
pub enum Filter<T> {
    All,
    Only(Vec<T>),
}

impl<T: PartialEq> Filter<T> {
    pub fn matches(&self, item: &T) -> bool {
        match self {
            Filter::All => true,
            Filter::Only(items) => items.contains(item),
        }
    }

    pub fn is_all(&self) -> bool {
        matches!(self, Filter::All)
    }

    /// The listed values, or an empty slice when the filter is `*`.
    pub fn values(&self) -> &[T] {
        match self {
            Filter::All => &[],
            Filter::Only(items) => items,
        }
    }
}

impl<T: Serialize> Serialize for Filter<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Filter::All => serializer.serialize_str("*"),
            Filter::Only(items) => items.serialize(serializer),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Filter<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw<T> {
            Star(String),
            Items(Vec<T>),
        }

        match Raw::<T>::deserialize(deserializer)? {
            Raw::Star(star) if star == "*" => Ok(Filter::All),
            Raw::Star(other) => Err(serde::de::Error::custom(format!(
                "expected '*' or a list, got {other:?}"
            ))),
            Raw::Items(items) => Ok(Filter::Only(items)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub quiet: bool,
    pub verbose: bool,

    // Kubernetes settings
    pub clusters: Option<Filter<String>>,
    pub kubeconfig: Option<String>,
    pub impersonate_user: Option<String>,
    pub impersonate_group: Option<String>,
    pub namespaces: Filter<String>,
    pub resources: Filter<Kind>,
    pub selector: Option<String>,

    // Recommendation value settings
    /// Minimum recommended CPU, in millicores.
    pub cpu_min_value: u32,
    /// Minimum recommended memory, in megabytes.
    pub memory_min_value: u32,

    // Prometheus settings
    pub prometheus_url: Option<String>,
    pub prometheus_auth_header: Option<String>,
    pub prometheus_other_headers: BTreeMap<String, String>,
    pub prometheus_ssl_enabled: bool,
    pub prometheus_cluster_label: Option<String>,
    pub prometheus_label: Option<String>,
    pub openshift: bool,

    // Concurrency
    pub max_workers: usize,

    // Output settings
    pub format: String,
    pub show_cluster_name: bool,
    pub show_severity: bool,
    pub strategy: String,
    pub log_to_stderr: bool,
    pub width: Option<usize>,
    pub file_output: Option<String>,
    pub file_output_dynamic: bool,

    /// Strategy specific arguments, kept for output parity with upstream.
    pub other_args: BTreeMap<String, serde_json::Value>,

    pub inside_cluster: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            quiet: false,
            verbose: false,
            clusters: None,
            kubeconfig: None,
            impersonate_user: None,
            impersonate_group: None,
            namespaces: Filter::All,
            resources: Filter::All,
            selector: None,
            cpu_min_value: 10,
            memory_min_value: 100,
            prometheus_url: None,
            prometheus_auth_header: None,
            prometheus_other_headers: BTreeMap::new(),
            prometheus_ssl_enabled: false,
            prometheus_cluster_label: None,
            prometheus_label: None,
            openshift: false,
            max_workers: 10,
            format: "table".to_string(),
            show_cluster_name: false,
            show_severity: true,
            strategy: "simple".to_string(),
            log_to_stderr: false,
            width: None,
            file_output: None,
            file_output_dynamic: false,
            other_args: BTreeMap::new(),
            inside_cluster: false,
        }
    }
}

impl Config {
    /// The kube context to use: the first explicitly named cluster, if any.
    pub fn context(&self) -> Option<&str> {
        match self.clusters.as_ref()? {
            Filter::All => None,
            Filter::Only(clusters) => clusters.first().map(String::as_str),
        }
    }

    /// The label used to tell clusters apart in a centralized Prometheus,
    /// rendered as a PromQL selector fragment (`, cluster="prod"`).
    pub fn prometheus_cluster_selector(&self) -> String {
        match (&self.prometheus_label, &self.prometheus_cluster_label) {
            (Some(label), Some(cluster)) => format!(", {label}=\"{cluster}\""),
            _ => String::new(),
        }
    }
}

/// Normalizes and validates a `--prometheus-url` value.
pub fn parse_prometheus_url(url: &str) -> Result<String> {
    if !url.starts_with("http://") && !url.starts_with("https://") {
        bail!("--prometheus-url must start with https:// or http://");
    }
    Ok(url.trim_end_matches('/').to_string())
}

/// Turns `--namespace` values into a filter, lowercasing them like upstream does.
pub fn parse_namespaces(values: &[String]) -> Result<Filter<String>> {
    if values.is_empty() || values.iter().any(|value| value == "*") {
        return Ok(Filter::All);
    }

    let mut namespaces = Vec::with_capacity(values.len());
    for value in values {
        if value.starts_with('*') {
            bail!("Namespace's values cannot start with an asterisk (*): {value}");
        }
        namespaces.push(value.to_lowercase());
    }
    Ok(Filter::Only(namespaces))
}

/// Turns `--resource` values into a filter of known workload kinds.
pub fn parse_resources(values: &[String]) -> Result<Filter<Kind>> {
    if values.is_empty() || values.iter().any(|value| value == "*") {
        return Ok(Filter::All);
    }

    let kinds = values
        .iter()
        .map(|value| {
            Kind::parse(value).with_context(|| {
                let known: Vec<&str> = Kind::ALL.iter().map(|kind| kind.as_str()).collect();
                format!(
                    "unknown resource '{value}', expected one of: {}",
                    known.join(", ")
                )
            })
        })
        .collect::<Result<Vec<Kind>>>()?;
    Ok(Filter::Only(kinds))
}

/// Parses `-H "X-Header: value"` arguments into a header map with lowercased names.
pub fn parse_headers(values: &[String]) -> Result<BTreeMap<String, String>> {
    values
        .iter()
        .map(|value| {
            let (name, header_value) = value
                .split_once(':')
                .with_context(|| format!("header must be formatted as 'key: value': {value}"))?;
            Ok((name.trim().to_lowercase(), header_value.trim().to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn prometheus_url_must_be_http() {
        assert!(parse_prometheus_url("prometheus:9090").is_err());
        assert_eq!(
            parse_prometheus_url("http://prom:9090/").unwrap(),
            "http://prom:9090"
        );
        assert_eq!(
            parse_prometheus_url("https://prom/x").unwrap(),
            "https://prom/x"
        );
    }

    #[test]
    fn namespaces_are_lowercased_and_validated() {
        assert_eq!(parse_namespaces(&[]).unwrap(), Filter::All);
        assert_eq!(parse_namespaces(&strings(&["*"])).unwrap(), Filter::All);
        assert_eq!(
            parse_namespaces(&strings(&["Default", "KUBE-SYSTEM"])).unwrap(),
            Filter::Only(strings(&["default", "kube-system"]))
        );
        assert!(parse_namespaces(&strings(&["*abc"])).is_err());
    }

    #[test]
    fn resources_are_matched_case_insensitively() {
        assert_eq!(parse_resources(&[]).unwrap(), Filter::All);
        assert_eq!(
            parse_resources(&strings(&["deployment", "DAEMONSET"])).unwrap(),
            Filter::Only(vec![Kind::Deployment, Kind::DaemonSet])
        );
        assert!(parse_resources(&strings(&["bogus"])).is_err());
    }

    #[test]
    fn headers_are_split_and_normalized() {
        let headers = parse_headers(&strings(&["X-Foo: bar", " X-Baz :qux "])).unwrap();
        assert_eq!(headers["x-foo"], "bar");
        assert_eq!(headers["x-baz"], "qux");
        assert!(parse_headers(&strings(&["nocolon"])).is_err());
    }

    #[test]
    fn filter_serializes_as_star_or_list() {
        let all: Filter<String> = Filter::All;
        assert_eq!(serde_json::to_string(&all).unwrap(), r#""*""#);
        assert_eq!(
            serde_json::to_string(&Filter::Only(strings(&["a"]))).unwrap(),
            r#"["a"]"#
        );

        let parsed: Filter<String> = serde_json::from_str(r#""*""#).unwrap();
        assert_eq!(parsed, Filter::All);
        let parsed: Filter<String> = serde_json::from_str(r#"["a"]"#).unwrap();
        assert_eq!(parsed, Filter::Only(strings(&["a"])));
        // an empty list stays empty here; "empty means all" is applied by the CLI parsers
        let parsed: Filter<String> = serde_json::from_str("[]").unwrap();
        assert_eq!(parsed, Filter::Only(vec![]));
    }

    #[test]
    fn filter_matches_items() {
        let filter = Filter::Only(vec![Kind::Job]);
        assert!(filter.matches(&Kind::Job));
        assert!(!filter.matches(&Kind::Deployment));
        assert!(Filter::All.matches(&Kind::Deployment));
    }

    #[test]
    fn context_is_the_first_named_cluster() {
        let mut config = Config::default();
        assert_eq!(config.context(), None);

        config.clusters = Some(Filter::All);
        assert_eq!(config.context(), None);

        config.clusters = Some(Filter::Only(strings(&["prod", "dev"])));
        assert_eq!(config.context(), Some("prod"));
    }

    #[test]
    fn cluster_selector_is_a_promql_fragment() {
        let mut config = Config::default();
        assert_eq!(config.prometheus_cluster_selector(), "");

        config.prometheus_label = Some("cluster".into());
        config.prometheus_cluster_label = Some("prod".into());
        assert_eq!(config.prometheus_cluster_selector(), r#", cluster="prod""#);
    }

    #[test]
    fn config_defaults_round_trip_through_json() {
        let config = Config::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(config, parsed);
        assert!(json.contains(r#""namespaces":"*""#));
    }
}
