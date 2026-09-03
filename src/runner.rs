//! Orchestration: discover workloads, load their history, apply the strategy.
//!
//! Port of `robusta_krr/core/runner.py`.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};

use crate::config::Config;
use crate::format;
use crate::k8s::{ClusterLoader, list_clusters};
use crate::model::{
    ByResource, K8sObject, PodWarning, RecValue, ResourceAllocations, ResourceType, Selector,
};
use crate::prometheus::discovery;
use crate::prometheus::loader::MetricsLoader;
use crate::result::{ResourceScan, ScanResult, StrategyData};
use crate::strategy::{Strategy, StrategyRecommendation};

/// Everything needed to scan one cluster.
struct ClusterContext {
    workloads: ClusterLoader,
    metrics: MetricsLoader,
}

pub struct Runner {
    config: Config,
    strategy: Box<dyn Strategy>,
    errors: Vec<serde_json::Value>,
}

impl Runner {
    pub fn new(config: Config, strategy: Box<dyn Strategy>) -> Self {
        Runner {
            config,
            strategy,
            errors: Vec::new(),
        }
    }

    /// Runs a scan and prints the report. Returns the process exit code.
    pub async fn run(&mut self) -> i32 {
        self.greet();

        match self.collect().await {
            Ok(result) => match self.report(&result) {
                Ok(()) => 0,
                Err(error) => {
                    tracing::error!("{error:#}");
                    1
                }
            },
            Err(error) => {
                tracing::error!("{error:#}");
                1
            }
        }
    }

    fn greet(&self) {
        if self.config.quiet {
            return;
        }
        eprintln!(
            "Running KRR (Kubernetes Resource Recommender) {}",
            env!("CARGO_PKG_VERSION")
        );
        eprintln!("Using strategy: {}", self.strategy.name());
        eprintln!("Using formatter: {}\n", self.config.format);
    }

    /// Scans every cluster and assembles the report.
    async fn collect(&mut self) -> Result<ScanResult> {
        let (clusters, inside_cluster) = self.resolve_clusters()?;
        self.config.inside_cluster = inside_cluster;

        if clusters.len() > 1 && self.config.prometheus_url.is_some() {
            bail!(
                "Cannot scan multiple clusters against a single Prometheus. \
                 Rerun with `-c <cluster>` where <cluster> is one of {}",
                clusters.join(", ")
            );
        }

        tracing::info!(
            "Using clusters: {}",
            if inside_cluster {
                "inner cluster".to_string()
            } else {
                clusters.join(", ")
            }
        );

        let mut contexts = Vec::new();
        for cluster in &clusters {
            let name = (!inside_cluster).then(|| cluster.clone());
            match self.connect(name.clone()).await {
                Ok(context) => contexts.push(context),
                Err(error) => tracing::error!("skipping cluster {cluster}: {error:#}"),
            }
        }

        if contexts.is_empty() {
            bail!("Could not connect to any cluster.");
        }

        for context in &contexts {
            self.check_history(context).await;
        }

        let mut workloads = Vec::new();
        for context in &contexts {
            workloads.extend(
                context
                    .workloads
                    .list_scannable_objects()
                    .await
                    .with_context(|| "could not list workloads")?
                    .into_iter()
                    .map(|object| (object, context)),
            );
        }

        if workloads.is_empty() {
            if self.config.namespaces.is_all() {
                tracing::warn!(
                    "Note that the '*' namespace filter excludes {} by default.",
                    "kube-system"
                );
            }
            bail!("Current filters resulted in no objects available to scan.");
        }

        let cluster_summary = match contexts.as_slice() {
            [single] => single.metrics.cluster_summary().await,
            _ => BTreeMap::new(),
        };

        let progress = self.progress_bar(workloads.len() as u64);
        let runner: &Runner = self;
        let scans: Vec<Option<ResourceScan>> = futures::stream::iter(workloads)
            .map(|(object, context)| {
                let progress = progress.clone();
                async move {
                    let scan = runner.scan(object, context).await;
                    if let Some(progress) = &progress {
                        progress.inc(1);
                    }
                    scan
                }
            })
            .buffer_unordered(self.config.max_workers)
            .collect()
            .await;
        if let Some(progress) = &progress {
            progress.finish_and_clear();
        }

        let mut scans: Vec<ResourceScan> = scans.into_iter().flatten().collect();
        if scans.is_empty() {
            bail!("No successful scans were made. Check the logs for more information.");
        }
        scans.sort_by(|a, b| {
            (
                &a.object.cluster,
                &a.object.namespace,
                &a.object.name,
                &a.object.container,
            )
                .cmp(&(
                    &b.object.cluster,
                    &b.object.namespace,
                    &b.object.name,
                    &b.object.container,
                ))
        });

        let strategy = StrategyData {
            name: self.strategy.name().to_string(),
            settings: self.strategy.settings_json(),
        };
        let mut result = ScanResult::new(scans, strategy, self.config.clone());
        result.description = Some(format!(
            "{} Strategy\n\n{}",
            title_case(self.strategy.name()),
            self.strategy.description()
        ));
        result.errors = std::mem::take(&mut self.errors);
        result.cluster_summary = cluster_summary;
        Ok(result)
    }

    /// Loads the history of one workload and applies the strategy.
    async fn scan(&self, mut object: K8sObject, context: &ClusterContext) -> Option<ResourceScan> {
        let settings = self.strategy.common();

        object.pods = match context.metrics.load_pods(&object, settings.history()).await {
            Ok(pods) => pods,
            Err(error) => {
                tracing::warn!("failed to load pods for {object}: {error:#}");
                Vec::new()
            }
        };

        if object.pods.is_empty() {
            // fast jobs may already be gone from Prometheus but still known to Kubernetes
            match context.workloads.list_pods(&object).await {
                Ok(pods) if !pods.is_empty() => {
                    tracing::warn!(
                        "Was not able to load any pods for {object} from Prometheus. \
                         Loaded pods from the Kubernetes API instead."
                    );
                    object.pods = pods;
                    object.add_warning(PodWarning::NoPrometheusPods);
                }
                Ok(_) => {}
                Err(error) => tracing::warn!("failed to list pods for {object}: {error:#}"),
            }
        }

        let metrics = self.strategy.metrics();
        let data = context
            .metrics
            .gather(&object, &metrics, settings.history(), settings.timeframe())
            .await;

        for (resource, warning) in [
            (ResourceType::Cpu, PodWarning::NoPrometheusCPUMetrics),
            (ResourceType::Memory, PodWarning::NoPrometheusMemoryMetrics),
        ] {
            let needle = match resource {
                ResourceType::Cpu => "cpu",
                ResourceType::Memory => "memory",
            };
            let expected: Vec<_> = metrics
                .iter()
                .filter(|metric| metric.key().to_lowercase().contains(needle))
                .collect();
            let loaded = expected
                .iter()
                .any(|metric| data.contains_key(metric.key()));
            if !expected.is_empty() && !loaded {
                object.add_warning(warning);
            }
        }

        let recommendation = self.strategy.run(&data, &object);
        tracing::info!(
            "Calculated recommendations for {object} (using {} metrics)",
            data.len()
        );
        Some(ResourceScan::calculate(
            object,
            self.allocations(recommendation),
        ))
    }

    /// Rounds the strategy output and applies the configured minimums.
    fn allocations(
        &self,
        recommendation: ByResource<StrategyRecommendation>,
    ) -> ResourceAllocations {
        ResourceAllocations {
            requests: ByResource::from_fn(|resource| {
                round_value(recommendation[resource].request, resource, &self.config)
            }),
            limits: ByResource::from_fn(|resource| {
                round_value(recommendation[resource].limit, resource, &self.config)
            }),
            info: ByResource::from_fn(|resource| recommendation[resource].info.clone()),
        }
    }

    /// Warns when the store holds less history than the strategy needs.
    async fn check_history(&mut self, context: &ClusterContext) {
        let settings = self.strategy.common();
        let range = match context
            .metrics
            .history_range(std::time::Duration::from_secs(5 * 3600))
            .await
        {
            Ok(range) => range,
            Err(_) => {
                tracing::info!(
                    "Unable to check how much historical data is available. Will assume it is \
                     sufficient and calculate recommendations anyway. (Not all Prometheus \
                     compatible metric stores support checking history settings.)"
                );
                self.errors
                    .push(serde_json::json!({"name": "HistoryRangeError"}));
                return;
            }
        };

        let available = std::time::Duration::from_secs_f64((range.1 - range.0).max(0.0));
        if settings.history_range_enough(available) {
            return;
        }

        let try_after = range.0 + settings.history().as_secs_f64();
        tracing::warn!(
            "Not enough history available. If the cluster was installed recently, it might take \
             some time for enough data to be collected. Enough data is estimated to be available \
             after unix time {try_after:.0}, but recommendations are calculated anyway."
        );
        self.errors.push(
            serde_json::json!({"name": "NotEnoughHistoryAvailable", "retry_after": try_after}),
        );
    }

    /// Clusters to scan and whether KRR runs inside one.
    fn resolve_clusters(&self) -> Result<(Vec<String>, bool)> {
        let kubeconfig = match &self.config.kubeconfig {
            Some(path) => kube::config::Kubeconfig::read_from(path),
            None => kube::config::Kubeconfig::read(),
        };

        match kubeconfig {
            Ok(kubeconfig) => {
                let contexts: Vec<String> = kubeconfig
                    .contexts
                    .iter()
                    .map(|context| context.name.clone())
                    .collect();
                let current = kubeconfig.current_context.clone();
                match list_clusters(&self.config, &contexts, current.as_deref()) {
                    Some(clusters) if !clusters.is_empty() => Ok((clusters, false)),
                    _ => bail!(
                        "Could not determine which cluster to scan. \
                         Pass -c with a context name from your kubeconfig."
                    ),
                }
            }
            Err(error) => {
                tracing::debug!("no kubeconfig ({error}), assuming we run inside the cluster");
                Ok((vec!["inner cluster".to_string()], true))
            }
        }
    }

    /// Builds the Kubernetes and metrics clients for one cluster.
    async fn connect(&self, cluster: Option<String>) -> Result<ClusterContext> {
        let client = self.kube_client(cluster.as_deref()).await?;
        let metrics = discovery::connect(&self.config, &client).await?;
        tracing::info!(
            "{} connected successfully for {} cluster",
            metrics.client.name,
            cluster.as_deref().unwrap_or("default")
        );

        Ok(ClusterContext {
            workloads: ClusterLoader::new(client, cluster, self.config.clone()),
            metrics,
        })
    }

    async fn kube_client(&self, context: Option<&str>) -> Result<kube::Client> {
        let options = kube::config::KubeConfigOptions {
            context: context.map(String::from),
            ..Default::default()
        };

        let mut config = match &self.config.kubeconfig {
            Some(path) => {
                let kubeconfig = kube::config::Kubeconfig::read_from(path)
                    .with_context(|| format!("could not read the kubeconfig at {path}"))?;
                kube::Config::from_custom_kubeconfig(kubeconfig, &options).await?
            }
            None => match kube::Config::from_kubeconfig(&options).await {
                Ok(config) => config,
                Err(error) => {
                    tracing::debug!("could not read the kubeconfig ({error}), trying in-cluster");
                    kube::Config::incluster()?
                }
            },
        };

        config.auth_info.impersonate = self.config.impersonate_user.clone();
        config.auth_info.impersonate_groups = self
            .config
            .impersonate_group
            .clone()
            .map(|group| vec![group]);

        Ok(kube::Client::try_from(config)?)
    }

    fn progress_bar(&self, total: u64) -> Option<Arc<ProgressBar>> {
        if self.config.quiet || !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
            return None;
        }

        let bar = ProgressBar::new(total);
        bar.set_style(
            ProgressStyle::with_template("Calculating recommendations [{bar:40}] {pos}/{len}")
                .expect("valid template")
                .progress_chars("=> "),
        );
        Some(Arc::new(bar))
    }

    /// Prints the report and writes it to a file when asked to.
    fn report(&self, result: &ScanResult) -> Result<()> {
        let formatter = format::find(&self.config.format)?;
        let output = (formatter.render)(result)?;
        println!("{output}");

        if let Some(path) = self.output_path() {
            // colored output is only meant for the terminal
            let file_output = if formatter.colored {
                let plain = format::find("table")?;
                (plain.render)(result)?
            } else {
                output
            };
            tracing::info!("Writing output to file: {path}");
            std::fs::write(&path, strip_ansi(&file_output))
                .with_context(|| format!("could not write {path}"))?;
        }
        Ok(())
    }

    fn output_path(&self) -> Option<String> {
        if self.config.file_output_dynamic {
            let stamp = chrono::Local::now().format("%Y%m%d%H%M%S");
            return Some(format!("krr-{stamp}.{}", self.config.format));
        }
        self.config.file_output.clone()
    }
}

/// Rounds a recommendation up to the smallest unit and applies the configured minimum.
pub fn round_value(value: RecValue, resource: ResourceType, config: &Config) -> RecValue {
    let Some(value) = value.as_f64() else {
        return value;
    };

    let (precision, minimum) = match resource {
        // the smallest CPU unit is 1m
        ResourceType::Cpu => (1000.0, config.cpu_min_value as f64 / 1000.0),
        // the smallest memory unit is 1MiB
        ResourceType::Memory => {
            let mib = 1024.0 * 1024.0;
            (1.0 / mib, config.memory_min_value as f64 * mib)
        }
    };

    let rounded = (value * precision).ceil() / precision;
    RecValue::Val(rounded.max(minimum))
}

/// "simple_limit" -> "Simple_Limit", as upstream titles the description.
fn title_case(name: &str) -> String {
    name.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Removes ANSI escapes so files never carry terminal colors.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Selectors of an allocation, exposed for tests of the rounding rules.
pub const SELECTORS: [Selector; 2] = Selector::ALL;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_is_rounded_up_to_millicores() {
        let config = Config::default();
        assert_eq!(
            round_value(RecValue::Val(0.0651), ResourceType::Cpu, &config),
            RecValue::Val(0.066)
        );
        assert_eq!(
            round_value(RecValue::Val(1.0), ResourceType::Cpu, &config),
            RecValue::Val(1.0)
        );
    }

    #[test]
    fn memory_is_rounded_up_to_mebibytes() {
        let config = Config::default();
        let mib = 1024.0 * 1024.0;
        assert_eq!(
            round_value(RecValue::Val(200.5 * mib), ResourceType::Memory, &config),
            RecValue::Val(201.0 * mib)
        );
    }

    #[test]
    fn minimums_are_applied() {
        let config = Config {
            cpu_min_value: 10,
            memory_min_value: 100,
            ..Default::default()
        };
        assert_eq!(
            round_value(RecValue::Val(0.001), ResourceType::Cpu, &config),
            RecValue::Val(0.01)
        );
        assert_eq!(
            round_value(RecValue::Val(1.0), ResourceType::Memory, &config),
            RecValue::Val(100.0 * 1024.0 * 1024.0)
        );
    }

    #[test]
    fn unknown_and_unset_values_are_left_alone() {
        let config = Config::default();
        assert_eq!(
            round_value(RecValue::Unset, ResourceType::Cpu, &config),
            RecValue::Unset
        );
        assert_eq!(
            round_value(RecValue::Unknown, ResourceType::Cpu, &config),
            RecValue::Unknown
        );
    }

    #[test]
    fn strategy_names_are_titled() {
        assert_eq!(title_case("simple"), "Simple");
        assert_eq!(title_case("simple_limit"), "Simple Limit");
    }

    #[test]
    fn ansi_escapes_are_stripped_for_files() {
        assert_eq!(strip_ansi("\u{1b}[31mred\u{1b}[0m text"), "red text");
    }
}
