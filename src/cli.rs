//! Command line interface.
//!
//! Port of `robusta_krr/main.py`, which generates one command per strategy.

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};

use crate::config::{self, Config};
use crate::format;
use crate::strategy::{CommonSettings, StrategyArgs};

#[derive(Debug, Parser)]
#[command(
    name = "krr",
    about = "Kubernetes Resource Recommender: right-size requests and limits from real usage",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// CPU request from a usage percentile, memory from the peak plus a buffer.
    Simple(SimpleArgs),
    /// Like simple, but the CPU limit is set from a higher percentile.
    #[command(alias = "simple_limit")]
    SimpleLimit(SimpleLimitArgs),
    /// Print the version and exit.
    Version,
}

#[derive(Debug, Args)]
pub struct SimpleArgs {
    #[command(flatten)]
    pub common: RunArgs,

    /// Percentile of CPU usage to request.
    #[arg(long, default_value_t = 95.0, help_heading = "Strategy Settings")]
    pub cpu_percentile: f64,
}

#[derive(Debug, Args)]
pub struct SimpleLimitArgs {
    #[command(flatten)]
    pub common: RunArgs,

    /// Percentile of CPU usage to request.
    #[arg(long, default_value_t = 66.0, help_heading = "Strategy Settings")]
    pub cpu_request: f64,

    /// Percentile of CPU usage to allow as a limit.
    #[arg(long, default_value_t = 96.0, help_heading = "Strategy Settings")]
    pub cpu_limit: f64,
}

/// Flags shared by every strategy.
#[derive(Debug, Args)]
pub struct RunArgs {
    // ---------------------------- Kubernetes ----------------------------
    /// Path to the kubeconfig file. Defaults to the usual locations.
    #[arg(short = 'k', long, help_heading = "Kubernetes Settings")]
    pub kubeconfig: Option<String>,

    /// Impersonate a user, like `kubectl --as`.
    #[arg(long = "as", help_heading = "Kubernetes Settings")]
    pub impersonate_user: Option<String>,

    /// Impersonate a group, like `kubectl --as-group`.
    #[arg(long = "as-group", help_heading = "Kubernetes Settings")]
    pub impersonate_group: Option<String>,

    /// Kube contexts to scan. Defaults to the current one.
    #[arg(
        short = 'c',
        long = "context",
        alias = "cluster",
        help_heading = "Kubernetes Settings"
    )]
    pub clusters: Vec<String>,

    /// Scan every context of the kubeconfig. Overrides --context.
    #[arg(long, help_heading = "Kubernetes Settings")]
    pub all_clusters: bool,

    /// Namespaces to scan; regular expressions are expanded. Defaults to all but kube-system.
    #[arg(short = 'n', long = "namespace", help_heading = "Kubernetes Settings")]
    pub namespaces: Vec<String>,

    /// Workload kinds to scan (Deployment, StatefulSet, ...). Case insensitive.
    #[arg(short = 'r', long = "resource", help_heading = "Kubernetes Settings")]
    pub resources: Vec<String>,

    /// Label selector applied to the workloads, e.g. -s key1=value1,key2=value2.
    #[arg(short = 's', long, help_heading = "Kubernetes Settings")]
    pub selector: Option<String>,

    // ---------------------------- Prometheus ----------------------------
    /// Prometheus URL. Discovered in the cluster when not given.
    #[arg(short = 'p', long, help_heading = "Prometheus Settings")]
    pub prometheus_url: Option<String>,

    /// Value of the Authorization header sent to Prometheus.
    #[arg(long, help_heading = "Prometheus Settings")]
    pub prometheus_auth_header: Option<String>,

    /// Additional headers, formatted as 'key: value'.
    #[arg(
        short = 'H',
        long = "prometheus-headers",
        help_heading = "Prometheus Settings"
    )]
    pub prometheus_other_headers: Vec<String>,

    /// Accept self-signed certificates when talking to Prometheus.
    #[arg(long, help_heading = "Prometheus Settings")]
    pub prometheus_ssl_enabled: bool,

    /// Value of the cluster label in a centralized Prometheus.
    #[arg(short = 'l', long, help_heading = "Prometheus Settings")]
    pub prometheus_cluster_label: Option<String>,

    /// Name of the label that tells clusters apart in a centralized Prometheus.
    #[arg(long, help_heading = "Prometheus Settings")]
    pub prometheus_label: Option<String>,

    /// Read the service account token and send it to Prometheus (OpenShift).
    #[arg(long, help_heading = "Prometheus Settings")]
    pub openshift: bool,

    // -------------------------- Recommendation --------------------------
    /// Lowest CPU KRR will recommend, in millicores.
    #[arg(
        long = "cpu-min",
        default_value_t = 10,
        help_heading = "Recommendation Settings"
    )]
    pub cpu_min_value: u32,

    /// Lowest memory KRR will recommend, in megabytes.
    #[arg(
        long = "mem-min",
        default_value_t = 100,
        help_heading = "Recommendation Settings"
    )]
    pub memory_min_value: u32,

    // ----------------------------- Discovery -----------------------------
    /// Page size for listing jobs.
    #[arg(long, default_value_t = 5000, help_heading = "Job Discovery Settings")]
    pub discovery_job_batch_size: u32,

    /// Maximum number of job pages to read.
    #[arg(long, default_value_t = 100, help_heading = "Job Discovery Settings")]
    pub discovery_job_max_batches: u32,

    /// Label name(s) grouping jobs into one GroupedJob workload, comma separated.
    #[arg(long, help_heading = "Job Grouping Settings")]
    pub job_grouping_labels: Option<String>,

    /// Maximum number of jobs and pods queried per group.
    #[arg(long, default_value_t = 500, help_heading = "Job Grouping Settings")]
    pub job_grouping_limit: usize,

    /// Workloads scanned at the same time.
    #[arg(
        short = 'w',
        long,
        default_value_t = 10,
        help_heading = "Threading Settings"
    )]
    pub max_workers: usize,

    // ------------------------- Strategy settings -------------------------
    /// Hours of history to look at.
    #[arg(long, default_value_t = 24.0 * 7.0 * 2.0, help_heading = "Strategy Settings")]
    pub history_duration: f64,

    /// Resolution of the history, in minutes.
    #[arg(long, default_value_t = 1.25, help_heading = "Strategy Settings")]
    pub timeframe_duration: f64,

    /// Headroom added to the peak memory usage, in percent.
    #[arg(long, default_value_t = 15.0, help_heading = "Strategy Settings")]
    pub memory_buffer_percentage: f64,

    /// Data points needed before a recommendation is made.
    #[arg(long, default_value_t = 100, help_heading = "Strategy Settings")]
    pub points_required: u32,

    /// Recommend for workloads that have an HPA as well.
    #[arg(long, help_heading = "Strategy Settings")]
    pub allow_hpa: bool,

    /// Raise the memory recommendation when OOMKills were seen (experimental).
    #[arg(long, help_heading = "Strategy Settings")]
    pub use_oomkill_data: bool,

    /// Headroom added to an OOMKilled memory limit, in percent.
    #[arg(long, default_value_t = 25.0, help_heading = "Strategy Settings")]
    pub oom_memory_buffer_percentage: f64,

    // ------------------------------ Output ------------------------------
    /// Output format.
    #[arg(
        short = 'f',
        long = "formatter",
        default_value = "table",
        help_heading = "Output Settings"
    )]
    pub format: String,

    /// Show the cluster column even for a single cluster.
    #[arg(long, help_heading = "Output Settings")]
    pub show_cluster_name: bool,

    /// Leave the severity out of the csv output.
    #[arg(long, help_heading = "Output Settings")]
    pub exclude_severity: bool,

    /// Write the output to this file as well.
    #[arg(long = "fileoutput", help_heading = "Output Settings")]
    pub file_output: Option<String>,

    /// Write the output to krr-{datetime}.{format} in the working directory.
    #[arg(long = "fileoutput-dynamic", help_heading = "Output Settings")]
    pub file_output_dynamic: bool,

    /// Width of the table output. Defaults to the terminal width.
    #[arg(long, help_heading = "Logging Settings")]
    pub width: Option<usize>,

    /// Log more.
    #[arg(short = 'v', long, help_heading = "Logging Settings")]
    pub verbose: bool,

    /// Log nothing but the result.
    #[arg(short = 'q', long, help_heading = "Logging Settings")]
    pub quiet: bool,

    /// Send logs to stderr instead of stdout.
    #[arg(long = "logtostderr", help_heading = "Logging Settings")]
    pub log_to_stderr: bool,
}

impl Command {
    /// Name of the strategy this command runs.
    pub fn strategy_name(&self) -> &'static str {
        match self {
            Command::Simple(_) => "simple",
            Command::SimpleLimit(_) => "simple_limit",
            Command::Version => "",
        }
    }

    /// Turns the arguments into a configuration and the strategy settings.
    pub fn into_settings(self) -> Result<(Config, StrategyArgs)> {
        let strategy = self.strategy_name().to_string();

        let (args, strategy_args) = match self {
            Command::Simple(args) => {
                let common = args.common.common_settings();
                let strategy_args = StrategyArgs {
                    common,
                    cpu_percentile: args.cpu_percentile,
                    ..Default::default()
                };
                (args.common, strategy_args)
            }
            Command::SimpleLimit(args) => {
                let common = args.common.common_settings();
                let strategy_args = StrategyArgs {
                    common,
                    cpu_request: args.cpu_request,
                    cpu_limit: args.cpu_limit,
                    ..Default::default()
                };
                (args.common, strategy_args)
            }
            Command::Version => bail!("the version command takes no settings"),
        };

        let config = args.into_config(strategy, &strategy_args)?;
        Ok((config, strategy_args))
    }
}

impl RunArgs {
    fn common_settings(&self) -> CommonSettings {
        CommonSettings {
            history_duration: self.history_duration,
            timeframe_duration: self.timeframe_duration,
            memory_buffer_percentage: self.memory_buffer_percentage,
            points_required: self.points_required,
            allow_hpa: self.allow_hpa,
            use_oomkill_data: self.use_oomkill_data,
            oom_memory_buffer_percentage: self.oom_memory_buffer_percentage,
        }
    }

    fn into_config(self, strategy: String, strategy_args: &StrategyArgs) -> Result<Config> {
        if self.exclude_severity && self.format != "csv" {
            bail!("--exclude-severity works only with --formatter csv");
        }
        format::find(&self.format)?;

        let prometheus_url = match &self.prometheus_url {
            Some(url) => Some(config::parse_prometheus_url(url)?),
            None => None,
        };

        let clusters = if self.all_clusters {
            Some(config::Filter::All)
        } else if self.clusters.is_empty() {
            None
        } else {
            Some(config::Filter::Only(self.clusters.clone()))
        };

        Ok(Config {
            quiet: self.quiet,
            verbose: self.verbose,
            clusters,
            kubeconfig: self.kubeconfig,
            impersonate_user: self.impersonate_user,
            impersonate_group: self.impersonate_group,
            namespaces: config::parse_namespaces(&self.namespaces)?,
            resources: config::parse_resources(&self.resources)?,
            selector: self.selector,
            cpu_min_value: self.cpu_min_value,
            memory_min_value: self.memory_min_value,
            prometheus_url,
            prometheus_auth_header: self.prometheus_auth_header,
            prometheus_other_headers: config::parse_headers(&self.prometheus_other_headers)?,
            prometheus_ssl_enabled: self.prometheus_ssl_enabled,
            prometheus_cluster_label: self.prometheus_cluster_label,
            prometheus_label: self.prometheus_label,
            openshift: self.openshift,
            discovery_job_batch_size: self.discovery_job_batch_size.max(1),
            discovery_job_max_batches: self.discovery_job_max_batches.max(1),
            job_grouping_labels: config::parse_grouping_labels(self.job_grouping_labels.as_deref()),
            job_grouping_limit: self.job_grouping_limit.max(1),
            max_workers: self.max_workers.max(1),
            format: self.format,
            show_cluster_name: self.show_cluster_name,
            show_severity: !self.exclude_severity,
            strategy,
            log_to_stderr: self.log_to_stderr,
            width: self.width,
            file_output: self.file_output,
            file_output_dynamic: self.file_output_dynamic,
            other_args: strategy_settings(strategy_args),
            inside_cluster: false,
        })
    }
}

/// Strategy arguments as they appear in the report's `config.other_args`.
fn strategy_settings(args: &StrategyArgs) -> std::collections::BTreeMap<String, serde_json::Value> {
    let mut settings = std::collections::BTreeMap::new();
    if let Ok(serde_json::Value::Object(common)) = serde_json::to_value(&args.common) {
        settings.extend(common);
    }
    settings.insert(
        "cpu_percentile".into(),
        serde_json::json!(args.cpu_percentile),
    );
    settings.insert("cpu_request".into(), serde_json::json!(args.cpu_request));
    settings.insert("cpu_limit".into(), serde_json::json!(args.cpu_limit));
    settings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Filter;
    use crate::model::Kind;

    fn parse(args: &[&str]) -> Result<(Config, StrategyArgs)> {
        Cli::try_parse_from(args)?.command.into_settings()
    }

    #[test]
    fn defaults_match_upstream() {
        let (config, strategy) = parse(&["krr", "simple"]).unwrap();

        assert_eq!(config.strategy, "simple");
        assert_eq!(config.format, "table");
        assert_eq!(config.namespaces, Filter::All);
        assert_eq!(config.resources, Filter::All);
        assert_eq!(config.clusters, None);
        assert_eq!(config.cpu_min_value, 10);
        assert_eq!(config.memory_min_value, 100);
        assert_eq!(config.max_workers, 10);
        assert!(config.show_severity);

        assert_eq!(strategy.cpu_percentile, 95.0);
        assert_eq!(strategy.common.history_duration, 336.0);
        assert_eq!(strategy.common.timeframe_duration, 1.25);
        assert_eq!(strategy.common.points_required, 100);
        assert!(!strategy.common.allow_hpa);
    }

    #[test]
    fn simple_limit_has_its_own_percentiles() {
        let (config, strategy) = parse(&[
            "krr",
            "simple_limit",
            "--cpu-request",
            "50",
            "--cpu-limit",
            "99",
        ])
        .unwrap();

        assert_eq!(config.strategy, "simple_limit");
        assert_eq!(strategy.cpu_request, 50.0);
        assert_eq!(strategy.cpu_limit, 99.0);
    }

    #[test]
    fn namespaces_and_resources_are_normalized() {
        let (config, _) = parse(&[
            "krr",
            "simple",
            "-n",
            "Default",
            "-n",
            "Prod",
            "-r",
            "deployment",
            "-r",
            "JOB",
        ])
        .unwrap();

        assert_eq!(
            config.namespaces,
            Filter::Only(vec!["default".into(), "prod".into()])
        );
        assert_eq!(
            config.resources,
            Filter::Only(vec![Kind::Deployment, Kind::Job])
        );
    }

    #[test]
    fn clusters_are_a_list_or_all() {
        let (config, _) = parse(&["krr", "simple", "-c", "prod", "-c", "dev"]).unwrap();
        assert_eq!(
            config.clusters,
            Some(Filter::Only(vec!["prod".into(), "dev".into()]))
        );
        assert_eq!(config.context(), Some("prod"));

        let (config, _) = parse(&["krr", "simple", "--all-clusters"]).unwrap();
        assert_eq!(config.clusters, Some(Filter::All));
    }

    #[test]
    fn prometheus_settings_are_validated() {
        let (config, _) = parse(&[
            "krr",
            "simple",
            "-p",
            "http://prom:9090/",
            "-H",
            "X-Scope-OrgID: tenant",
            "--prometheus-label",
            "cluster",
            "-l",
            "prod",
        ])
        .unwrap();

        assert_eq!(config.prometheus_url.as_deref(), Some("http://prom:9090"));
        assert_eq!(config.prometheus_other_headers["x-scope-orgid"], "tenant");
        assert_eq!(config.prometheus_cluster_selector(), r#", cluster="prod""#);

        assert!(parse(&["krr", "simple", "-p", "prom:9090"]).is_err());
        assert!(parse(&["krr", "simple", "-H", "nocolon"]).is_err());
    }

    #[test]
    fn job_discovery_and_grouping_are_configurable() {
        let (config, _) = parse(&["krr", "simple"]).unwrap();
        assert_eq!(config.discovery_job_batch_size, 5000);
        assert_eq!(config.discovery_job_max_batches, 100);
        assert_eq!(config.job_grouping_labels, None);
        assert_eq!(config.job_grouping_limit, 500);

        let (config, _) = parse(&[
            "krr",
            "simple",
            "--job-grouping-labels",
            "app,team",
            "--job-grouping-limit",
            "50",
            "--discovery-job-batch-size",
            "1000",
        ])
        .unwrap();
        assert_eq!(
            config.job_grouping_labels,
            Some(vec!["app".into(), "team".into()])
        );
        assert_eq!(config.job_grouping_limit, 50);
        assert_eq!(config.discovery_job_batch_size, 1000);
    }

    #[test]
    fn unknown_formatters_are_rejected() {
        assert!(parse(&["krr", "simple", "-f", "nope"]).is_err());
        assert!(parse(&["krr", "simple", "-f", "json"]).is_ok());
    }

    #[test]
    fn excluding_severity_needs_csv() {
        assert!(parse(&["krr", "simple", "--exclude-severity"]).is_err());

        let (config, _) = parse(&["krr", "simple", "-f", "csv", "--exclude-severity"]).unwrap();
        assert!(!config.show_severity);
    }

    #[test]
    fn strategy_flags_reach_the_settings() {
        let (config, strategy) = parse(&[
            "krr",
            "simple",
            "--cpu-percentile",
            "90",
            "--history-duration",
            "24",
            "--timeframe-duration",
            "0.5",
            "--allow-hpa",
            "--use-oomkill-data",
        ])
        .unwrap();

        assert_eq!(strategy.cpu_percentile, 90.0);
        assert_eq!(strategy.common.history_duration, 24.0);
        assert_eq!(strategy.common.timeframe_duration, 0.5);
        assert!(strategy.common.allow_hpa);
        assert!(strategy.common.use_oomkill_data);

        // the report repeats them
        assert_eq!(config.other_args["cpu_percentile"], serde_json::json!(90.0));
        assert_eq!(
            config.other_args["history_duration"],
            serde_json::json!(24.0)
        );
    }

    #[test]
    fn the_cli_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
