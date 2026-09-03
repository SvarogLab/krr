//! Finding a metrics store in the cluster.
//!
//! Port of `robusta_krr/utils/service_discovery.py` and the `*_metrics_service` classes.

use std::collections::BTreeMap;

use anyhow::Result;
use k8s_openapi::api::core::v1::Service;
use k8s_openapi::api::networking::v1::Ingress;
use kube::api::{Api, ListParams};

use crate::config::Config;

use super::client::PrometheusClient;
use super::loader::MetricsLoader;

/// A metrics store KRR knows how to find.
pub struct StoreKind {
    pub name: &'static str,
    /// Label selectors to look for, in order.
    pub selectors: &'static [&'static str],
    /// Appended to the discovered URL.
    pub url_postfix: &'static str,
    /// Headers the store needs.
    pub headers: &'static [(&'static str, &'static str)],
}

/// Stores are probed in this order, as upstream does.
pub const STORES: &[StoreKind] = &[
    StoreKind {
        name: "Victoria Metrics",
        selectors: &[
            "app.kubernetes.io/name=vmsingle",
            "app.kubernetes.io/name=victoria-metrics-single",
            "app.kubernetes.io/name=vmselect",
            "app.kubernetes.io/component=vmselect",
            "app=vmselect",
        ],
        url_postfix: "",
        headers: &[],
    },
    StoreKind {
        name: "Thanos",
        selectors: &[
            "app.kubernetes.io/component=query,app.kubernetes.io/name=thanos",
            "app.kubernetes.io/name=thanos-query",
            "app=thanos-query",
            "app=thanos-querier",
        ],
        url_postfix: "",
        headers: &[],
    },
    StoreKind {
        name: "Mimir",
        selectors: &["app.kubernetes.io/name=mimir,app.kubernetes.io/component=query-frontend"],
        url_postfix: "/prometheus",
        headers: &[("x-scope-orgid", "anonymous")],
    },
    StoreKind {
        name: "Prometheus",
        selectors: &[
            "app=kube-prometheus-stack-prometheus",
            "app=prometheus,component=server",
            "app=prometheus-server",
            "app=prometheus-operator-prometheus",
            "app=rancher-monitoring-prometheus",
            "app=prometheus-prometheus",
            "app.kubernetes.io/name=prometheus,app.kubernetes.io/component=server",
            "app=stack-prometheus",
        ],
        url_postfix: "",
        headers: &[],
    },
];

/// Where a discovered service can be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    /// Reachable over the network, e.g. from inside the cluster.
    Direct(String),
    /// Only reachable through the Kubernetes API server's service proxy.
    Proxy(String),
}

/// The endpoint of a service, as upstream builds it.
pub fn service_endpoint(service: &Service, inside_cluster: bool) -> Option<Endpoint> {
    let name = service.metadata.name.as_ref()?;
    let namespace = service.metadata.namespace.as_ref()?;
    let port = service.spec.as_ref()?.ports.as_ref()?.first()?.port;

    if inside_cluster {
        Some(Endpoint::Direct(format!(
            "http://{name}.{namespace}.svc.cluster.local:{port}"
        )))
    } else {
        Some(Endpoint::Proxy(format!(
            "/api/v1/namespaces/{namespace}/services/{name}:{port}/proxy"
        )))
    }
}

/// The host an ingress exposes, used when KRR runs outside the cluster.
pub fn ingress_endpoint(ingress: &Ingress) -> Option<Endpoint> {
    let host = ingress
        .spec
        .as_ref()?
        .rules
        .as_ref()?
        .first()?
        .host
        .as_ref()?;
    Some(Endpoint::Direct(format!("http://{host}")))
}

/// Looks for a service, then an ingress, matching any of the selectors.
pub async fn find_endpoint(
    client: &kube::Client,
    selectors: &[&str],
    inside_cluster: bool,
) -> Option<Endpoint> {
    let services: Api<Service> = Api::all(client.clone());
    let ingresses: Api<Ingress> = Api::all(client.clone());

    for selector in selectors {
        tracing::debug!("looking for a service matching {selector}");
        let params = ListParams::default().labels(selector);
        if let Ok(list) = services.list(&params).await
            && let Some(endpoint) = list
                .items
                .first()
                .and_then(|s| service_endpoint(s, inside_cluster))
        {
            tracing::debug!("found a service matching {selector}");
            return Some(endpoint);
        }

        if inside_cluster {
            continue;
        }

        if let Ok(list) = ingresses.list(&params).await
            && let Some(endpoint) = list.items.first().and_then(ingress_endpoint)
        {
            tracing::debug!("found an ingress matching {selector}");
            return Some(endpoint);
        }
    }

    None
}

/// Builds the metrics loader for a cluster: either the configured URL or a discovered store.
pub async fn connect(config: &Config, client: &kube::Client) -> Result<MetricsLoader> {
    let cluster_selector = config.prometheus_cluster_selector();

    if let Some(url) = &config.prometheus_url {
        tracing::info!("using the Prometheus URL from the command line: {url}");
        let client = PrometheusClient::direct(
            "Prometheus",
            url,
            &headers(config, &[]),
            config.prometheus_ssl_enabled,
        )?;
        client.check_connection().await?;
        return Ok(MetricsLoader::new(client, cluster_selector));
    }

    tracing::info!("no Prometheus URL given, looking for a metrics store in the cluster");
    for store in STORES {
        let Some(endpoint) = find_endpoint(client, store.selectors, config.inside_cluster).await
        else {
            continue;
        };

        let prometheus = match &endpoint {
            Endpoint::Direct(url) => PrometheusClient::direct(
                store.name,
                &format!("{url}{}", store.url_postfix),
                &headers(config, store.headers),
                config.prometheus_ssl_enabled,
            )?,
            Endpoint::Proxy(path) => PrometheusClient::kube_proxy(
                store.name,
                client.clone(),
                &format!("{path}{}", store.url_postfix),
            ),
        };

        match prometheus.check_connection().await {
            Ok(()) => {
                tracing::info!("{} found", store.name);
                return Ok(MetricsLoader::new(prometheus, cluster_selector));
            }
            Err(error) => tracing::info!("{} not usable: {error:#}", store.name),
        }
    }

    anyhow::bail!(
        "Wasn't able to connect to any Prometheus service in the cluster.\n\
         Try using port-forwarding and/or setting the url manually (using the -p flag).\n\
         For more information, see 'Giving the Explicit Prometheus URL' at \
         https://github.com/robusta-dev/krr?tab=readme-ov-file#usage"
    )
}

/// Where the service account token lives in a pod.
const SERVICE_ACCOUNT_TOKEN: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// Headers sent to the store: the store's own, plus the configured ones.
fn headers(config: &Config, extra: &[(&str, &str)]) -> BTreeMap<String, String> {
    let mut headers: BTreeMap<String, String> = extra
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();
    headers.extend(config.prometheus_other_headers.clone());

    let mut auth = config.prometheus_auth_header.clone();
    if auth.is_none() && config.openshift {
        tracing::info!("Openshift flag is set, trying to load the token from the service account");
        match read_token(SERVICE_ACCOUNT_TOKEN) {
            Some(token) => {
                tracing::info!("Openshift token loaded successfully");
                auth = Some(format!("Bearer {token}"));
            }
            None => tracing::warn!("Openshift token not found, trying to connect without it"),
        }
    }

    if let Some(auth) = auth {
        headers.insert("authorization".to_string(), auth);
    }
    headers
}

/// Reads a service account token, ignoring surrounding whitespace.
fn read_token(path: &str) -> Option<String> {
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{ServicePort, ServiceSpec};
    use k8s_openapi::api::networking::v1::{IngressRule, IngressSpec};
    use kube::api::ObjectMeta;

    fn service() -> Service {
        Service {
            metadata: ObjectMeta {
                name: Some("prometheus".into()),
                namespace: Some("monitoring".into()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                ports: Some(vec![ServicePort {
                    port: 9090,
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            status: None,
        }
    }

    #[test]
    fn in_cluster_services_are_reached_directly() {
        assert_eq!(
            service_endpoint(&service(), true),
            Some(Endpoint::Direct(
                "http://prometheus.monitoring.svc.cluster.local:9090".into()
            ))
        );
    }

    #[test]
    fn outside_the_cluster_services_go_through_the_api_proxy() {
        assert_eq!(
            service_endpoint(&service(), false),
            Some(Endpoint::Proxy(
                "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy".into()
            ))
        );
    }

    #[test]
    fn services_without_ports_are_skipped() {
        let mut service = service();
        service.spec.as_mut().unwrap().ports = None;
        assert_eq!(service_endpoint(&service, true), None);
    }

    #[test]
    fn ingresses_expose_a_host() {
        let ingress = Ingress {
            metadata: ObjectMeta::default(),
            spec: Some(IngressSpec {
                rules: Some(vec![IngressRule {
                    host: Some("prom.example.com".into()),
                    http: None,
                }]),
                ..Default::default()
            }),
            status: None,
        };
        assert_eq!(
            ingress_endpoint(&ingress),
            Some(Endpoint::Direct("http://prom.example.com".into()))
        );
    }

    #[test]
    fn store_order_matches_upstream() {
        let names: Vec<&str> = STORES.iter().map(|store| store.name).collect();
        assert_eq!(
            names,
            vec!["Victoria Metrics", "Thanos", "Mimir", "Prometheus"]
        );
        assert_eq!(STORES[2].url_postfix, "/prometheus");
        assert_eq!(STORES[2].headers, &[("x-scope-orgid", "anonymous")]);
    }

    #[test]
    fn the_openshift_token_becomes_a_bearer_header() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        std::fs::write(&path, "  secret-token\n").unwrap();

        assert_eq!(
            read_token(path.to_str().unwrap()).as_deref(),
            Some("secret-token")
        );
        assert_eq!(read_token("/does/not/exist"), None);

        // an explicit header wins over the service account token
        let mut config = Config {
            openshift: true,
            ..Default::default()
        };
        config.prometheus_auth_header = Some("Bearer explicit".into());
        assert_eq!(headers(&config, &[])["authorization"], "Bearer explicit");

        // without a token file there is simply no header
        let config = Config {
            openshift: true,
            ..Default::default()
        };
        assert!(!headers(&config, &[]).contains_key("authorization"));
    }

    #[test]
    fn headers_combine_store_defaults_with_configuration() {
        let config = Config {
            prometheus_auth_header: Some("Bearer token".into()),
            prometheus_other_headers: BTreeMap::from([("x-custom".into(), "1".into())]),
            ..Default::default()
        };

        let headers = headers(&config, &[("x-scope-orgid", "anonymous")]);
        assert_eq!(headers["x-scope-orgid"], "anonymous");
        assert_eq!(headers["x-custom"], "1");
        assert_eq!(headers["authorization"], "Bearer token");
    }
}
