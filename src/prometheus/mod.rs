//! Reading usage history from Prometheus and its compatible stores.

pub mod client;
pub mod loader;
pub mod metrics;

use std::collections::BTreeMap;

/// Samples of one metric, per pod: `pod -> [(timestamp, value)]`.
pub type PodSeries = BTreeMap<String, Vec<(f64, f64)>>;

/// All metrics loaded for a workload, keyed by [`metrics::Metric::key`].
pub type MetricsData = BTreeMap<String, PodSeries>;
