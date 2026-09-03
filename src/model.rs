//! Core data model: resources, allocations and scannable Kubernetes objects.
//!
//! Port of `robusta_krr/core/models/{allocations,objects}.py`.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Index, IndexMut};

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::units;

pub const NONE_LITERAL: &str = "unset";
pub const NAN_LITERAL: &str = "?";

/// The kinds of resources KRR recommends for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceType {
    Cpu,
    Memory,
}

impl ResourceType {
    pub const ALL: [ResourceType; 2] = [ResourceType::Cpu, ResourceType::Memory];

    /// Lowercase key used in serialized output ("cpu").
    pub fn key(self) -> &'static str {
        match self {
            ResourceType::Cpu => "cpu",
            ResourceType::Memory => "memory",
        }
    }

    /// Human readable name used in column headers ("CPU", "Memory").
    pub fn name(self) -> &'static str {
        match self {
            ResourceType::Cpu => "CPU",
            ResourceType::Memory => "Memory",
        }
    }
}

/// A recommended or currently allocated value.
///
/// Mirrors python's `Union[float, Literal["?"], None]`: `Unset` means "should not be set",
/// `Unknown` means "the strategy could not tell".
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum RecValue {
    #[default]
    Unset,
    Unknown,
    Val(f64),
}

impl RecValue {
    /// `NaN` maps to `Unknown`, like pydantic's validator does.
    pub fn from_f64(value: f64) -> Self {
        if value.is_nan() {
            RecValue::Unknown
        } else {
            RecValue::Val(value)
        }
    }

    pub fn from_option(value: Option<f64>) -> Self {
        match value {
            None => RecValue::Unset,
            Some(value) => RecValue::from_f64(value),
        }
    }

    /// The numeric value, or `0.0` for non-numeric ones (matches upstream diff math).
    pub fn number_or_zero(self) -> f64 {
        self.as_f64().unwrap_or(0.0)
    }

    pub fn as_f64(self) -> Option<f64> {
        match self {
            RecValue::Val(value) => Some(value),
            _ => None,
        }
    }
}

impl Serialize for RecValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            RecValue::Unset => serializer.serialize_none(),
            RecValue::Unknown => serializer.serialize_str(NAN_LITERAL),
            RecValue::Val(value) if value.is_nan() => serializer.serialize_str(NAN_LITERAL),
            RecValue::Val(value) => serializer.serialize_f64(*value),
        }
    }
}

impl<'de> Deserialize<'de> for RecValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RecValueVisitor;

        impl<'de> Visitor<'de> for RecValueVisitor {
            type Value = RecValue;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a resource quantity, null or \"?\"")
            }

            fn visit_unit<E: de::Error>(self) -> Result<RecValue, E> {
                Ok(RecValue::Unset)
            }

            fn visit_none<E: de::Error>(self) -> Result<RecValue, E> {
                Ok(RecValue::Unset)
            }

            fn visit_some<D: Deserializer<'de>>(
                self,
                deserializer: D,
            ) -> Result<RecValue, D::Error> {
                deserializer.deserialize_any(RecValueVisitor)
            }

            fn visit_f64<E: de::Error>(self, value: f64) -> Result<RecValue, E> {
                Ok(RecValue::from_f64(value))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<RecValue, E> {
                Ok(RecValue::Val(value as f64))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<RecValue, E> {
                Ok(RecValue::Val(value as f64))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<RecValue, E> {
                if value == NAN_LITERAL {
                    return Ok(RecValue::Unknown);
                }
                units::parse(value)
                    .map(RecValue::from_f64)
                    .map_err(|_| E::custom(format!("invalid resource quantity: {value}")))
            }
        }

        deserializer.deserialize_any(RecValueVisitor)
    }
}

/// Renders a value for humans: `unset`, `?` or a quantity.
pub fn format_recommendation_value(value: RecValue) -> String {
    match value {
        RecValue::Unset => NONE_LITERAL.to_string(),
        RecValue::Unknown => NAN_LITERAL.to_string(),
        RecValue::Val(value) => units::format(value),
    }
}

/// Which side of the allocation we are looking at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector {
    Requests,
    Limits,
}

impl Selector {
    pub const ALL: [Selector; 2] = [Selector::Requests, Selector::Limits];
}

/// A pair of values, one per resource type. Serializes as `{"cpu": .., "memory": ..}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ByResource<T> {
    pub cpu: T,
    pub memory: T,
}

impl<T> ByResource<T> {
    pub fn from_fn(mut f: impl FnMut(ResourceType) -> T) -> Self {
        ByResource {
            cpu: f(ResourceType::Cpu),
            memory: f(ResourceType::Memory),
        }
    }
}

impl<T> Index<ResourceType> for ByResource<T> {
    type Output = T;
    fn index(&self, resource: ResourceType) -> &T {
        match resource {
            ResourceType::Cpu => &self.cpu,
            ResourceType::Memory => &self.memory,
        }
    }
}

impl<T> IndexMut<ResourceType> for ByResource<T> {
    fn index_mut(&mut self, resource: ResourceType) -> &mut T {
        match resource {
            ResourceType::Cpu => &mut self.cpu,
            ResourceType::Memory => &mut self.memory,
        }
    }
}

/// Requests/limits of a container, or of a recommendation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ResourceAllocations {
    pub requests: ByResource<RecValue>,
    pub limits: ByResource<RecValue>,
    #[serde(default)]
    pub info: ByResource<Option<String>>,
}

impl ResourceAllocations {
    pub fn get(&self, selector: Selector) -> &ByResource<RecValue> {
        match selector {
            Selector::Requests => &self.requests,
            Selector::Limits => &self.limits,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodData {
    pub name: String,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HpaData {
    pub min_replicas: Option<i32>,
    pub max_replicas: i32,
    pub current_replicas: Option<i32>,
    pub desired_replicas: i32,
    pub target_cpu_utilization_percentage: Option<f64>,
    pub target_memory_utilization_percentage: Option<f64>,
}

impl HpaData {
    /// The HPA target for a resource, if any.
    pub fn target_for(&self, resource: ResourceType) -> Option<f64> {
        match resource {
            ResourceType::Cpu => self.target_cpu_utilization_percentage,
            ResourceType::Memory => self.target_memory_utilization_percentage,
        }
    }
}

/// Workload kinds KRR can scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Kind {
    Deployment,
    DaemonSet,
    StatefulSet,
    Job,
    CronJob,
    Rollout,
    DeploymentConfig,
    StrimziPodSet,
    /// Jobs sharing a label value, scanned as one workload.
    GroupedJob,
}

impl Kind {
    pub const ALL: [Kind; 9] = [
        Kind::Deployment,
        Kind::DaemonSet,
        Kind::StatefulSet,
        Kind::Job,
        Kind::CronJob,
        Kind::Rollout,
        Kind::DeploymentConfig,
        Kind::StrimziPodSet,
        Kind::GroupedJob,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Deployment => "Deployment",
            Kind::DaemonSet => "DaemonSet",
            Kind::StatefulSet => "StatefulSet",
            Kind::Job => "Job",
            Kind::CronJob => "CronJob",
            Kind::Rollout => "Rollout",
            Kind::DeploymentConfig => "DeploymentConfig",
            Kind::StrimziPodSet => "StrimziPodSet",
            Kind::GroupedJob => "GroupedJob",
        }
    }

    /// Case insensitive lookup, used by `--resource`.
    pub fn parse(name: &str) -> Option<Kind> {
        Kind::ALL
            .into_iter()
            .find(|kind| kind.as_str().eq_ignore_ascii_case(name))
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum PodWarning {
    NoPrometheusPods,
    NoPrometheusCPUMetrics,
    NoPrometheusMemoryMetrics,
}

/// A single scannable container of a workload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct K8sObject {
    /// `None` means KRR runs inside the cluster.
    #[serde(default)]
    pub cluster: Option<String>,
    pub name: String,
    pub container: String,
    #[serde(default)]
    pub pods: Vec<PodData>,
    #[serde(default)]
    pub hpa: Option<HpaData>,
    pub namespace: String,
    pub kind: Kind,
    pub allocations: ResourceAllocations,
    #[serde(default)]
    pub warnings: BTreeSet<PodWarning>,
    #[serde(default)]
    pub labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub annotations: Option<BTreeMap<String, String>>,
    /// Label selector used to find the pods of this workload; not serialized.
    #[serde(skip)]
    pub pod_selector: Option<String>,
    /// UID of the owning API resource (used to find the jobs of a CronJob); not serialized.
    #[serde(skip)]
    pub uid: Option<String>,
    /// Names of the jobs a `GroupedJob` stands for; not serialized.
    #[serde(skip)]
    pub grouped_jobs: Vec<String>,
}

impl K8sObject {
    pub fn current_pods_count(&self) -> usize {
        self.pods.iter().filter(|pod| !pod.deleted).count()
    }

    pub fn deleted_pods_count(&self) -> usize {
        self.pods.iter().filter(|pod| pod.deleted).count()
    }

    pub fn add_warning(&mut self, warning: PodWarning) {
        self.warnings.insert(warning);
    }

    /// Splits the pod list into chunks of at most `n` pods, cloning the rest of the object.
    pub fn split_into_batches(&self, n: usize) -> Vec<K8sObject> {
        if self.pods.len() <= n {
            return vec![self.clone()];
        }

        self.pods
            .chunks(n)
            .map(|batch| K8sObject {
                pods: batch.to_vec(),
                ..self.clone()
            })
            .collect()
    }
}

impl std::fmt::Display for K8sObject {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "{} {}/{}/{}",
            self.kind, self.namespace, self.name, self.container
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(pods: Vec<PodData>) -> K8sObject {
        K8sObject {
            cluster: Some("c".into()),
            name: "web".into(),
            container: "app".into(),
            pods,
            hpa: None,
            namespace: "default".into(),
            kind: Kind::Deployment,
            allocations: ResourceAllocations::default(),
            warnings: BTreeSet::new(),
            labels: None,
            annotations: None,
            pod_selector: None,
            uid: None,
            grouped_jobs: Vec::new(),
        }
    }

    fn pod(name: &str, deleted: bool) -> PodData {
        PodData {
            name: name.into(),
            deleted,
        }
    }

    #[test]
    fn resource_type_names() {
        assert_eq!(ResourceType::Cpu.key(), "cpu");
        assert_eq!(ResourceType::Cpu.name(), "CPU");
        assert_eq!(ResourceType::Memory.key(), "memory");
        assert_eq!(ResourceType::Memory.name(), "Memory");
    }

    #[test]
    fn nan_becomes_unknown() {
        assert_eq!(RecValue::from_f64(f64::NAN), RecValue::Unknown);
        assert_eq!(RecValue::from_f64(1.5), RecValue::Val(1.5));
        assert_eq!(RecValue::from_option(None), RecValue::Unset);
        assert_eq!(RecValue::Unknown.number_or_zero(), 0.0);
        assert_eq!(RecValue::Val(2.0).as_f64(), Some(2.0));
    }

    #[test]
    fn formats_values_for_humans() {
        assert_eq!(format_recommendation_value(RecValue::Unset), "unset");
        assert_eq!(format_recommendation_value(RecValue::Unknown), "?");
        assert_eq!(format_recommendation_value(RecValue::Val(0.05)), "50m");
    }

    #[test]
    fn by_resource_is_indexable() {
        let mut values = ByResource::from_fn(|r| match r {
            ResourceType::Cpu => 1,
            ResourceType::Memory => 2,
        });
        assert_eq!(values[ResourceType::Cpu], 1);
        assert_eq!(values[ResourceType::Memory], 2);
        values[ResourceType::Cpu] = 7;
        assert_eq!(values[ResourceType::Cpu], 7);
    }

    #[test]
    fn deserializes_allocations_with_units() {
        let allocations: ResourceAllocations = serde_json::from_str(
            r#"{"requests": {"cpu": "50m", "memory": "2048Mi"},
                "limits": {"cpu": 2.0, "memory": null},
                "info": {}}"#,
        )
        .unwrap();

        assert_eq!(allocations.requests[ResourceType::Cpu], RecValue::Val(0.05));
        assert_eq!(
            allocations.requests[ResourceType::Memory],
            RecValue::Val(2147483648.0)
        );
        assert_eq!(allocations.limits[ResourceType::Cpu], RecValue::Val(2.0));
        assert_eq!(allocations.limits[ResourceType::Memory], RecValue::Unset);
        assert_eq!(
            allocations.get(Selector::Requests)[ResourceType::Cpu],
            RecValue::Val(0.05)
        );
    }

    #[test]
    fn serializes_values_like_upstream() {
        let allocations = ResourceAllocations {
            requests: ByResource {
                cpu: RecValue::Val(0.5),
                memory: RecValue::Unknown,
            },
            limits: ByResource {
                cpu: RecValue::Unset,
                memory: RecValue::Val(1.0),
            },
            info: ByResource::default(),
        };
        let json = serde_json::to_value(&allocations).unwrap();
        assert_eq!(json["requests"]["cpu"], serde_json::json!(0.5));
        assert_eq!(json["requests"]["memory"], serde_json::json!("?"));
        assert_eq!(json["limits"]["cpu"], serde_json::Value::Null);
    }

    #[test]
    fn question_mark_deserializes_to_unknown() {
        let value: RecValue = serde_json::from_str(r#""?""#).unwrap();
        assert_eq!(value, RecValue::Unknown);
    }

    #[test]
    fn kind_parsing_is_case_insensitive() {
        assert_eq!(Kind::parse("deployment"), Some(Kind::Deployment));
        assert_eq!(Kind::parse("DAEMONSET"), Some(Kind::DaemonSet));
        assert_eq!(Kind::parse("groupedjob"), Some(Kind::GroupedJob));
        assert_eq!(Kind::parse("nope"), None);
        assert_eq!(Kind::Deployment.to_string(), "Deployment");
    }

    #[test]
    fn hpa_targets_per_resource() {
        let hpa = HpaData {
            min_replicas: Some(1),
            max_replicas: 5,
            current_replicas: Some(2),
            desired_replicas: 2,
            target_cpu_utilization_percentage: Some(80.0),
            target_memory_utilization_percentage: None,
        };
        assert_eq!(hpa.target_for(ResourceType::Cpu), Some(80.0));
        assert_eq!(hpa.target_for(ResourceType::Memory), None);
    }

    #[test]
    fn counts_pods_by_state() {
        let mut obj = object(vec![pod("a", false), pod("b", false), pod("c", true)]);
        assert_eq!(obj.current_pods_count(), 2);
        assert_eq!(obj.deleted_pods_count(), 1);
        assert_eq!(obj.to_string(), "Deployment default/web/app");

        obj.add_warning(PodWarning::NoPrometheusPods);
        obj.add_warning(PodWarning::NoPrometheusPods);
        assert_eq!(obj.warnings.len(), 1);
    }

    #[test]
    fn splits_pods_into_batches() {
        let obj = object(vec![pod("a", false), pod("b", false), pod("c", false)]);
        assert_eq!(obj.split_into_batches(5).len(), 1);

        let batches = obj.split_into_batches(2);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].pods.len(), 2);
        assert_eq!(batches[1].pods.len(), 1);
        assert_eq!(batches[1].name, "web");
    }
}
