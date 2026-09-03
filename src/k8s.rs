//! Discovering scannable workloads in a cluster.
//!
//! Port of `robusta_krr/core/integrations/kubernetes/__init__.py`.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use futures::future::join_all;
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::core::v1::{Container, Namespace, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::api::{Api, ApiResource, DynamicObject, ListParams, ObjectMeta};

use crate::config::{Config, Filter};
use crate::model::{
    ByResource, HpaData, K8sObject, Kind, PodData, RecValue, ResourceAllocations, ResourceType,
};
use crate::units;

/// Workloads in this namespace are skipped when no namespace filter is given.
const DEFAULT_SKIPPED_NAMESPACE: &str = "kube-system";

/// Characters that make a `--namespace` value a regular expression.
const REGEX_CHARS: &[char] = &['*', '|', '(', ')', '[', ']', '^', '$', '?', '+', '.', '\\'];

/// Key of the HPA index: namespace, target kind and target name.
pub type HpaKey = (String, String, String);
pub type HpaIndex = BTreeMap<HpaKey, HpaData>;

/// Custom resources KRR scans when their CRD is installed.
const CUSTOM_KINDS: &[(Kind, &str, &str, &str)] = &[
    (Kind::Rollout, "argoproj.io", "v1alpha1", "rollouts"),
    (
        Kind::DeploymentConfig,
        "apps.openshift.io",
        "v1",
        "deploymentconfigs",
    ),
    (
        Kind::StrimziPodSet,
        "core.strimzi.io",
        "v1beta2",
        "strimzipodsets",
    ),
];

/// Reads workloads from one cluster.
pub struct ClusterLoader {
    pub client: kube::Client,
    /// Name of the kube context, or `None` when running inside the cluster.
    pub cluster: Option<String>,
    pub config: Config,
}

impl ClusterLoader {
    pub fn new(client: kube::Client, cluster: Option<String>, config: Config) -> Self {
        ClusterLoader {
            client,
            cluster,
            config,
        }
    }

    /// Every container of every workload that passes the filters.
    pub async fn list_scannable_objects(&self) -> Result<Vec<K8sObject>> {
        let namespaces = self.namespaces().await;
        let hpas = self.list_hpas(&namespaces).await;

        let mut objects = Vec::new();

        objects.extend(
            self.collect::<Deployment>(Kind::Deployment, &namespaces, &hpas, |item| {
                let spec = item.spec.as_ref()?;
                Some((
                    containers(spec.template.spec.as_ref()),
                    Some(spec.selector.clone()),
                ))
            })
            .await,
        );
        objects.extend(
            self.collect::<StatefulSet>(Kind::StatefulSet, &namespaces, &hpas, |item| {
                let spec = item.spec.as_ref()?;
                Some((
                    containers(spec.template.spec.as_ref()),
                    Some(spec.selector.clone()),
                ))
            })
            .await,
        );
        objects.extend(
            self.collect::<DaemonSet>(Kind::DaemonSet, &namespaces, &hpas, |item| {
                let spec = item.spec.as_ref()?;
                Some((
                    containers(spec.template.spec.as_ref()),
                    Some(spec.selector.clone()),
                ))
            })
            .await,
        );
        objects.extend(
            self.collect::<Job>(Kind::Job, &namespaces, &hpas, |item| {
                // jobs of a cronjob are covered by their cronjob
                if owned_by_cron_job(&item.metadata) {
                    return None;
                }
                let spec = item.spec.as_ref()?;
                Some((
                    containers(spec.template.spec.as_ref()),
                    spec.selector.clone(),
                ))
            })
            .await,
        );
        objects.extend(
            self.collect::<CronJob>(Kind::CronJob, &namespaces, &hpas, |item| {
                let spec = item.spec.job_template.spec.as_ref()?;
                Some((
                    containers(spec.template.spec.as_ref()),
                    spec.selector.clone(),
                ))
            })
            .await,
        );

        for (kind, group, version, plural) in CUSTOM_KINDS {
            objects.extend(
                self.collect_custom(*kind, group, version, plural, &namespaces, &hpas)
                    .await,
            );
        }

        // by default kube-system is not scanned
        if namespaces.is_all() {
            objects.retain(|object| object.namespace != DEFAULT_SKIPPED_NAMESPACE);
        }
        Ok(objects)
    }

    /// Pods currently backing a workload, used when Prometheus knows none.
    pub async fn list_pods(&self, object: &K8sObject) -> Result<Vec<PodData>> {
        let selector = match object.kind {
            Kind::CronJob => match self.cron_job_pod_selector(object).await? {
                Some(selector) => selector,
                None => return Ok(Vec::new()),
            },
            _ => match &object.pod_selector {
                Some(selector) => selector.clone(),
                None => return Ok(Vec::new()),
            },
        };

        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &object.namespace);
        let params = ListParams::default().labels(&selector);
        Ok(pods
            .list(&params)
            .await?
            .items
            .into_iter()
            .filter_map(|pod| pod.metadata.name)
            .map(|name| PodData {
                name,
                deleted: false,
            })
            .collect())
    }

    /// Pods of a CronJob are found through the jobs it owns.
    async fn cron_job_pod_selector(&self, object: &K8sObject) -> Result<Option<String>> {
        let uid = object
            .uid
            .as_deref()
            .context("cronjob was loaded without a uid")?;
        let jobs: Api<Job> = Api::namespaced(self.client.clone(), &object.namespace);

        let owned: Vec<String> = jobs
            .list(&ListParams::default())
            .await?
            .items
            .into_iter()
            .filter(|job| {
                job.metadata
                    .owner_references
                    .iter()
                    .flatten()
                    .any(|owner| owner.kind == "CronJob" && owner.uid == uid)
            })
            .filter_map(|job| job.metadata.uid)
            .collect();

        if owned.is_empty() {
            return Ok(None);
        }
        Ok(Some(format!(
            "batch.kubernetes.io/controller-uid in ({})",
            owned.join(",")
        )))
    }

    /// HPAs of the cluster, indexed by the workload they scale.
    pub async fn list_hpas(&self, namespaces: &Filter<String>) -> HpaIndex {
        match self.list::<HorizontalPodAutoscaler>(namespaces).await {
            Ok(hpas) => hpas.iter().filter_map(hpa_entry).collect(),
            Err(error) => {
                tracing::warn!(
                    "could not list HPAs in {}: {error:#}. Assuming there are none, \
                     which may lead to inaccurate results for autoscaled workloads.",
                    self.cluster.as_deref().unwrap_or("the cluster")
                );
                HpaIndex::new()
            }
        }
    }

    /// Namespaces to scan, expanding regular expressions in `--namespace`.
    pub async fn namespaces(&self) -> Filter<String> {
        let requested = match &self.config.namespaces {
            Filter::All => return Filter::All,
            Filter::Only(values) => values,
        };

        let patterns: Vec<&String> = requested
            .iter()
            .filter(|value| value.contains(REGEX_CHARS))
            .collect();
        let mut namespaces: Vec<String> = requested
            .iter()
            .filter(|value| !value.contains(REGEX_CHARS))
            .cloned()
            .collect();

        if patterns.is_empty() {
            return Filter::Only(namespaces);
        }

        tracing::info!("found regex patterns in --namespace, expanding the namespace list");
        let api: Api<Namespace> = Api::all(self.client.clone());
        let all = match api.list(&ListParams::default()).await {
            Ok(list) => list
                .items
                .into_iter()
                .filter_map(|ns| ns.metadata.name)
                .collect::<Vec<_>>(),
            Err(error) => {
                tracing::error!("could not list namespaces: {error:#}");
                return Filter::Only(namespaces);
            }
        };

        namespaces.extend(expand_namespaces(&patterns, &all, &namespaces));
        Filter::Only(namespaces)
    }

    /// Lists one workload kind and turns every container into a scannable object.
    async fn collect<K>(
        &self,
        kind: Kind,
        namespaces: &Filter<String>,
        hpas: &HpaIndex,
        extract: impl Fn(&K) -> Option<(Vec<Container>, Option<LabelSelector>)>,
    ) -> Vec<K8sObject>
    where
        K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>
            + Clone
            + std::fmt::Debug
            + serde::de::DeserializeOwned,
        K::DynamicType: Default,
    {
        if !self.config.resources.matches(&kind) {
            tracing::debug!("skipping {kind}s");
            return Vec::new();
        }

        let items = match self.list::<K>(namespaces).await {
            Ok(items) => items,
            Err(error) => {
                tracing::error!(
                    "could not list {kind}s: {error:#}. Skipping this kind and continuing."
                );
                return Vec::new();
            }
        };

        items
            .iter()
            .filter_map(|item| {
                let (containers, selector) = extract(item)?;
                Some(self.build(item.meta(), &containers, selector.as_ref(), kind, hpas))
            })
            .flatten()
            .collect()
    }

    /// Lists a custom resource, tolerating clusters where its CRD is missing.
    async fn collect_custom(
        &self,
        kind: Kind,
        group: &str,
        version: &str,
        plural: &str,
        namespaces: &Filter<String>,
        hpas: &HpaIndex,
    ) -> Vec<K8sObject> {
        if !self.config.resources.matches(&kind) {
            return Vec::new();
        }

        let resource = ApiResource {
            group: group.to_string(),
            version: version.to_string(),
            api_version: format!("{group}/{version}"),
            kind: kind.as_str().to_string(),
            plural: plural.to_string(),
        };

        let params = self.list_params();
        let items = match namespaces {
            Filter::All => {
                Api::<DynamicObject>::all_with(self.client.clone(), &resource)
                    .list(&params)
                    .await
            }
            Filter::Only(namespaces) => {
                let mut items = Vec::new();
                let mut result = Ok(());
                for namespace in namespaces {
                    let api = Api::<DynamicObject>::namespaced_with(
                        self.client.clone(),
                        namespace,
                        &resource,
                    );
                    match api.list(&params).await {
                        Ok(list) => items.extend(list.items),
                        Err(error) => result = Err(error),
                    }
                }
                result.map(|_| kube::core::ObjectList {
                    items,
                    metadata: Default::default(),
                    types: Default::default(),
                })
            }
        };

        let items = match items {
            Ok(list) => list.items,
            Err(error) => {
                // the CRD is simply not installed in most clusters
                tracing::debug!("{kind} API not available: {error:#}");
                return Vec::new();
            }
        };

        items
            .iter()
            .filter_map(|item| {
                let value = serde_json::to_value(item).ok()?;
                let (containers, selector) = custom_workload_parts(&value, kind)?;
                Some(self.build(&item.metadata, &containers, selector.as_ref(), kind, hpas))
            })
            .flatten()
            .collect()
    }

    fn build(
        &self,
        meta: &ObjectMeta,
        containers: &[Container],
        selector: Option<&LabelSelector>,
        kind: Kind,
        hpas: &HpaIndex,
    ) -> Vec<K8sObject> {
        let pod_selector = selector.and_then(selector_query);
        scannable_objects(meta, containers, kind, self.cluster.as_deref(), hpas)
            .into_iter()
            .map(|object| K8sObject {
                pod_selector: pod_selector.clone(),
                uid: meta.uid.clone(),
                ..object
            })
            .collect()
    }

    fn list_params(&self) -> ListParams {
        match &self.config.selector {
            Some(selector) => ListParams::default().labels(selector),
            None => ListParams::default(),
        }
    }

    /// Lists resources in all namespaces, or in the configured ones.
    async fn list<K>(&self, namespaces: &Filter<String>) -> Result<Vec<K>>
    where
        K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>
            + Clone
            + std::fmt::Debug
            + serde::de::DeserializeOwned,
        K::DynamicType: Default,
    {
        let params = self.list_params();

        match namespaces {
            Filter::All => Ok(Api::<K>::all(self.client.clone())
                .list(&params)
                .await?
                .items),
            Filter::Only(namespaces) => {
                let requests = namespaces.iter().map(|namespace| {
                    let api = Api::<K>::namespaced(self.client.clone(), namespace);
                    let params = params.clone();
                    async move { api.list(&params).await }
                });

                let mut items = Vec::new();
                for result in join_all(requests).await {
                    items.extend(result?.items);
                }
                Ok(items)
            }
        }
    }
}

/// Kube contexts to scan, or `None` when running inside a cluster.
pub fn list_clusters(
    config: &Config,
    contexts: &[String],
    current: Option<&str>,
) -> Option<Vec<String>> {
    if config.inside_cluster {
        return None;
    }

    match &config.clusters {
        // no --context: the current one
        None => current.map(|current| vec![current.to_string()]),
        Some(Filter::Only(names)) if names.is_empty() => current.map(|c| vec![c.to_string()]),
        // --all-clusters
        Some(Filter::All) => Some(contexts.to_vec()),
        Some(Filter::Only(names)) => Some(
            contexts
                .iter()
                .filter(|context| names.contains(context))
                .cloned()
                .collect(),
        ),
    }
}

/// Namespaces matching any of the patterns, ignoring the ones already listed.
pub fn expand_namespaces(patterns: &[&String], all: &[String], already: &[String]) -> Vec<String> {
    let mut matched = Vec::new();
    for pattern in patterns {
        let regex = match regex::Regex::new(&format!("^(?:{pattern})$")) {
            Ok(regex) => regex,
            Err(error) => {
                tracing::error!("ignoring invalid namespace pattern {pattern}: {error}");
                continue;
            }
        };
        for namespace in all {
            if regex.is_match(namespace)
                && !already.contains(namespace)
                && !matched.contains(namespace)
            {
                matched.push(namespace.clone());
            }
        }
    }
    matched
}

/// Containers of a pod spec.
fn containers(spec: Option<&k8s_openapi::api::core::v1::PodSpec>) -> Vec<Container> {
    spec.map(|spec| spec.containers.clone()).unwrap_or_default()
}

fn owned_by_cron_job(meta: &ObjectMeta) -> bool {
    meta.owner_references
        .iter()
        .flatten()
        .any(|owner| owner.kind == "CronJob")
}

/// Builds one scannable object per container of a workload.
pub fn scannable_objects(
    meta: &ObjectMeta,
    containers: &[Container],
    kind: Kind,
    cluster: Option<&str>,
    hpas: &HpaIndex,
) -> Vec<K8sObject> {
    let name = meta.name.clone().unwrap_or_default();
    let namespace = meta.namespace.clone().unwrap_or_default();
    let hpa = hpas
        .get(&(namespace.clone(), kind.as_str().to_string(), name.clone()))
        .cloned();

    containers
        .iter()
        .map(|container| K8sObject {
            cluster: cluster.map(String::from),
            name: name.clone(),
            container: container.name.clone(),
            pods: Vec::new(),
            hpa: hpa.clone(),
            namespace: namespace.clone(),
            kind,
            allocations: allocations_of(container),
            warnings: Default::default(),
            labels: meta
                .labels
                .clone()
                .map(|labels| labels.into_iter().collect()),
            annotations: meta
                .annotations
                .clone()
                .map(|values| values.into_iter().collect()),
            pod_selector: None,
            uid: meta.uid.clone(),
        })
        .collect()
}

/// Reads the requests and limits of a container.
pub fn allocations_of(container: &Container) -> ResourceAllocations {
    let quantity = |values: Option<
        &BTreeMap<String, k8s_openapi::apimachinery::pkg::api::resource::Quantity>,
    >,
                    resource: ResourceType| {
        values
            .and_then(|values| values.get(resource.key()))
            .and_then(|quantity| units::parse(&quantity.0).ok())
            .map(RecValue::from_f64)
            .unwrap_or(RecValue::Unset)
    };

    let resources = container.resources.as_ref();
    ResourceAllocations {
        requests: ByResource::from_fn(|resource| {
            quantity(resources.and_then(|r| r.requests.as_ref()), resource)
        }),
        limits: ByResource::from_fn(|resource| {
            quantity(resources.and_then(|r| r.limits.as_ref()), resource)
        }),
        info: ByResource::default(),
    }
}

/// Renders a label selector the way `kubectl -l` expects it.
pub fn selector_query(selector: &LabelSelector) -> Option<String> {
    let mut filters: Vec<String> = selector
        .match_labels
        .iter()
        .flatten()
        .map(|(label, value)| format!("{label}={value}"))
        .collect();

    for expression in selector.match_expressions.iter().flatten() {
        filters.push(match expression.operator.to_lowercase().as_str() {
            "exists" => expression.key.clone(),
            "doesnotexist" => format!("!{}", expression.key),
            _ => format!(
                "{} {} ({})",
                expression.key,
                expression.operator,
                expression.values.clone().unwrap_or_default().join(",")
            ),
        });
    }

    (!filters.is_empty()).then(|| filters.join(","))
}

/// Indexes an HPA by the workload it scales.
pub fn hpa_entry(hpa: &HorizontalPodAutoscaler) -> Option<(HpaKey, HpaData)> {
    let spec = &hpa.spec;
    let status = hpa.status.as_ref();

    let target = |resource: &str| -> Option<f64> {
        spec.metrics.iter().flatten().find_map(|metric| {
            let source = metric.resource.as_ref()?;
            (metric.type_ == "Resource" && source.name == resource)
                .then(|| source.target.average_utilization.map(f64::from))?
        })
    };

    let key = (
        hpa.metadata.namespace.clone()?,
        spec.scale_target_ref.kind.clone(),
        spec.scale_target_ref.name.clone(),
    );
    let data = HpaData {
        min_replicas: spec.min_replicas,
        max_replicas: spec.max_replicas,
        current_replicas: status.and_then(|status| status.current_replicas),
        desired_replicas: status
            .map(|status| status.desired_replicas)
            .unwrap_or_default(),
        target_cpu_utilization_percentage: target("cpu"),
        target_memory_utilization_percentage: target("memory"),
    };
    Some((key, data))
}

/// Containers and pod selector of a custom resource, read from its JSON.
pub fn custom_workload_parts(
    value: &serde_json::Value,
    kind: Kind,
) -> Option<(Vec<Container>, Option<LabelSelector>)> {
    let spec = value.get("spec")?;
    let containers = match kind {
        // a StrimziPodSet carries full pod definitions instead of a template
        Kind::StrimziPodSet => spec.get("pods")?.get(0)?.get("spec")?.get("containers")?,
        _ => spec.get("template")?.get("spec")?.get("containers")?,
    };

    let containers: Vec<Container> = serde_json::from_value(containers.clone()).ok()?;
    let selector = spec
        .get("selector")
        .and_then(|s| serde_json::from_value(s.clone()).ok());
    Some((containers, selector))
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::autoscaling::v2::{
        CrossVersionObjectReference, HorizontalPodAutoscalerSpec, HorizontalPodAutoscalerStatus,
        MetricSpec, MetricTarget, ResourceMetricSource,
    };
    use k8s_openapi::api::core::v1::ResourceRequirements;
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement;

    fn meta(name: &str, namespace: &str) -> ObjectMeta {
        ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some(namespace.to_string()),
            uid: Some("uid-1".to_string()),
            labels: Some([("app".to_string(), "web".to_string())].into()),
            annotations: Some([("owner".to_string(), "team".to_string())].into()),
            ..Default::default()
        }
    }

    fn container(name: &str, requests: &[(&str, &str)], limits: &[(&str, &str)]) -> Container {
        let quantities = |pairs: &[(&str, &str)]| -> Option<BTreeMap<String, Quantity>> {
            (!pairs.is_empty()).then(|| {
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), Quantity(v.to_string())))
                    .collect()
            })
        };

        Container {
            name: name.to_string(),
            resources: Some(ResourceRequirements {
                requests: quantities(requests),
                limits: quantities(limits),
                claims: None,
            }),
            ..Default::default()
        }
    }

    fn hpa(kind: &str, name: &str, metrics: Vec<(&str, i32)>) -> HorizontalPodAutoscaler {
        HorizontalPodAutoscaler {
            metadata: meta("hpa", "default"),
            spec: HorizontalPodAutoscalerSpec {
                min_replicas: Some(1),
                max_replicas: 10,
                scale_target_ref: CrossVersionObjectReference {
                    kind: kind.to_string(),
                    name: name.to_string(),
                    api_version: None,
                },
                metrics: Some(
                    metrics
                        .into_iter()
                        .map(|(resource, utilization)| MetricSpec {
                            type_: "Resource".to_string(),
                            resource: Some(ResourceMetricSource {
                                name: resource.to_string(),
                                target: MetricTarget {
                                    average_utilization: Some(utilization),
                                    type_: "Utilization".to_string(),
                                    ..Default::default()
                                },
                            }),
                            ..Default::default()
                        })
                        .collect(),
                ),
                behavior: None,
            },
            status: Some(HorizontalPodAutoscalerStatus {
                current_replicas: Some(2),
                desired_replicas: 3,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn reads_container_allocations() {
        let allocations = allocations_of(&container(
            "app",
            &[("cpu", "50m"), ("memory", "1Gi")],
            &[("cpu", "2")],
        ));

        assert_eq!(allocations.requests[ResourceType::Cpu], RecValue::Val(0.05));
        assert_eq!(
            allocations.requests[ResourceType::Memory],
            RecValue::Val(1024.0 * 1024.0 * 1024.0)
        );
        assert_eq!(allocations.limits[ResourceType::Cpu], RecValue::Val(2.0));
        assert_eq!(allocations.limits[ResourceType::Memory], RecValue::Unset);
    }

    #[test]
    fn containers_without_resources_are_unset() {
        let allocations = allocations_of(&Container {
            name: "app".into(),
            ..Default::default()
        });
        assert_eq!(allocations.requests[ResourceType::Cpu], RecValue::Unset);
        assert_eq!(allocations.limits[ResourceType::Memory], RecValue::Unset);
    }

    #[test]
    fn one_scannable_object_per_container() {
        let objects = scannable_objects(
            &meta("web", "default"),
            &[
                container("app", &[("cpu", "50m")], &[]),
                container("sidecar", &[], &[]),
            ],
            Kind::Deployment,
            Some("prod"),
            &HpaIndex::new(),
        );

        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].name, "web");
        assert_eq!(objects[0].container, "app");
        assert_eq!(objects[0].namespace, "default");
        assert_eq!(objects[0].kind, Kind::Deployment);
        assert_eq!(objects[0].cluster.as_deref(), Some("prod"));
        assert_eq!(
            objects[0].allocations.requests[ResourceType::Cpu],
            RecValue::Val(0.05)
        );
        assert_eq!(objects[0].labels.as_ref().unwrap()["app"], "web");
        assert_eq!(objects[0].annotations.as_ref().unwrap()["owner"], "team");
        assert_eq!(objects[0].uid.as_deref(), Some("uid-1"));
        assert_eq!(objects[1].container, "sidecar");
    }

    #[test]
    fn workloads_are_matched_with_their_hpa() {
        let mut hpas = HpaIndex::new();
        let (key, data) = hpa_entry(&hpa("Deployment", "web", vec![("cpu", 80)])).unwrap();
        hpas.insert(key, data);

        let objects = scannable_objects(
            &meta("web", "default"),
            &[container("app", &[], &[])],
            Kind::Deployment,
            None,
            &hpas,
        );
        assert_eq!(
            objects[0]
                .hpa
                .as_ref()
                .unwrap()
                .target_cpu_utilization_percentage,
            Some(80.0)
        );

        // a StatefulSet of the same name is not scaled by that HPA
        let objects = scannable_objects(
            &meta("web", "default"),
            &[container("app", &[], &[])],
            Kind::StatefulSet,
            None,
            &hpas,
        );
        assert!(objects[0].hpa.is_none());
    }

    #[test]
    fn hpa_targets_are_read_per_resource() {
        let (key, data) =
            hpa_entry(&hpa("Deployment", "web", vec![("cpu", 80), ("memory", 70)])).unwrap();

        assert_eq!(
            key,
            (
                "default".to_string(),
                "Deployment".to_string(),
                "web".to_string()
            )
        );
        assert_eq!(data.target_cpu_utilization_percentage, Some(80.0));
        assert_eq!(data.target_memory_utilization_percentage, Some(70.0));
        assert_eq!(data.min_replicas, Some(1));
        assert_eq!(data.max_replicas, 10);
        assert_eq!(data.current_replicas, Some(2));
        assert_eq!(data.desired_replicas, 3);
    }

    #[test]
    fn selectors_render_labels_and_expressions() {
        let selector = LabelSelector {
            match_labels: Some([("app".to_string(), "web".to_string())].into()),
            match_expressions: Some(vec![
                LabelSelectorRequirement {
                    key: "tier".into(),
                    operator: "In".into(),
                    values: Some(vec!["front".into(), "back".into()]),
                },
                LabelSelectorRequirement {
                    key: "legacy".into(),
                    operator: "Exists".into(),
                    values: None,
                },
                LabelSelectorRequirement {
                    key: "beta".into(),
                    operator: "DoesNotExist".into(),
                    values: None,
                },
            ]),
        };

        let query = selector_query(&selector).unwrap();
        assert!(query.contains("app=web"), "{query}");
        assert!(query.contains("tier In (front,back)"), "{query}");
        assert!(query.contains("legacy"), "{query}");
        assert!(query.contains("!beta"), "{query}");
    }

    #[test]
    fn an_empty_selector_matches_nothing() {
        assert_eq!(selector_query(&LabelSelector::default()), None);
    }

    #[test]
    fn clusters_come_from_the_kubeconfig_contexts() {
        let contexts = ["prod".to_string(), "dev".to_string()];
        let mut config = Config::default();

        // no --context: the current one
        assert_eq!(
            list_clusters(&config, &contexts, Some("dev")),
            Some(vec!["dev".to_string()])
        );

        // --all-clusters
        config.clusters = Some(Filter::All);
        assert_eq!(
            list_clusters(&config, &contexts, Some("dev")),
            Some(contexts.to_vec())
        );

        // explicit contexts, unknown ones are dropped
        config.clusters = Some(Filter::Only(vec!["prod".to_string(), "nope".to_string()]));
        assert_eq!(
            list_clusters(&config, &contexts, Some("dev")),
            Some(vec!["prod".to_string()])
        );

        // inside the cluster there is no kubeconfig to read
        config.inside_cluster = true;
        assert_eq!(list_clusters(&config, &contexts, Some("dev")), None);
    }

    #[test]
    fn namespace_patterns_are_expanded() {
        let all = [
            "prod-a".to_string(),
            "prod-b".to_string(),
            "dev".to_string(),
        ];
        let pattern = "prod-.*".to_string();

        assert_eq!(
            expand_namespaces(&[&pattern], &all, &[]),
            vec!["prod-a", "prod-b"]
        );
        // namespaces already listed are not repeated
        assert_eq!(
            expand_namespaces(&[&pattern], &all, &["prod-a".to_string()]),
            vec!["prod-b".to_string()]
        );
        // patterns are anchored: "dev" does not match "prod-.*"
        let invalid = "[".to_string();
        assert!(expand_namespaces(&[&invalid], &all, &[]).is_empty());
    }

    #[test]
    fn custom_resources_expose_their_containers() {
        let rollout = serde_json::json!({
            "spec": {
                "selector": {"matchLabels": {"app": "web"}},
                "template": {"spec": {"containers": [{"name": "app", "resources": {"requests": {"cpu": "1"}}}]}}
            }
        });
        let (containers, selector) = custom_workload_parts(&rollout, Kind::Rollout).unwrap();
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0].name, "app");
        assert_eq!(selector_query(&selector.unwrap()).unwrap(), "app=web");

        let podset = serde_json::json!({
            "spec": {"pods": [{"spec": {"containers": [{"name": "kafka"}]}}]}
        });
        let (containers, _) = custom_workload_parts(&podset, Kind::StrimziPodSet).unwrap();
        assert_eq!(containers[0].name, "kafka");

        // a resource with an unexpected shape is skipped instead of failing
        assert!(custom_workload_parts(&serde_json::json!({"spec": {}}), Kind::Rollout).is_none());
    }
}
