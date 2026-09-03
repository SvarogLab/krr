# Porting notes

What this port is based on, what it covers, and what was left out on purpose.

## Upstream source

| | |
| --- | --- |
| Project | [robusta-dev/krr](https://github.com/robusta-dev/krr) (MIT) |
| Ported from commit | `d1ccf018da611590be8c620f1a9a6bcb0a9effd3` |
| Commit date | 2026-09-01 |
| Version at that commit | `1.8.2-dev` |

Only the `robusta_krr/` package is in scope. The `enforcer/` directory is a separate
in-cluster component, not part of the CLI.

### Checking for upstream changes

```sh
git clone --filter=blob:none https://github.com/robusta-dev/krr upstream
git -C upstream log --oneline d1ccf018da611590be8c620f1a9a6bcb0a9effd3..HEAD -- robusta_krr
git -C upstream diff d1ccf018da611590be8c620f1a9a6bcb0a9effd3..HEAD -- robusta_krr/strategies
```

After porting a newer change, update the commit above so the next comparison starts there.

## Module map

| Here | Upstream |
| --- | --- |
| `src/units.rs` | `utils/resource_units.py` |
| `src/model.rs` | `core/models/allocations.py`, `core/models/objects.py` |
| `src/severity.rs` | `core/models/severity.py` |
| `src/result.rs` | `core/models/result.py` |
| `src/config.rs` | `core/models/config.py` |
| `src/format/` | `core/abstract/formatters.py`, `formatters/*` |
| `src/strategy/` | `core/abstract/strategies.py`, `strategies/*` |
| `src/prometheus/metrics.rs` | `core/integrations/prometheus/metrics/*` |
| `src/prometheus/client.rs` | the `prometrix` / `prometheus_api_client` dependency |
| `src/prometheus/loader.rs` | `core/integrations/prometheus/loader.py`, `metrics_service/*` |
| `src/prometheus/discovery.rs` | `utils/service_discovery.py`, the `*_metrics_service` classes |
| `src/k8s.rs` | `core/integrations/kubernetes/__init__.py` |
| `src/runner.rs` | `core/runner.py` |
| `src/cli.rs` | `main.py` |

## What is implemented

**Workload kinds** — all nine: Deployment, StatefulSet, DaemonSet, Job, CronJob, plus
Argo Rollout, OpenShift DeploymentConfig and Strimzi PodSet when their CRDs are installed,
and GroupedJob (jobs sharing a label value, behind `--job-grouping-labels`).

**Metric stores** — Victoria Metrics, Thanos, Mimir and Prometheus, probed in that order by
label selector, reachable directly or through the Kubernetes API server's service proxy.
`--prometheus-url` skips discovery. Extra headers, an explicit `Authorization` header, the
OpenShift service-account token and self-signed certificates are supported.

**Strategies** — `simple` and `simple_limit`, with the same defaults, settings and guards as
upstream: percentile CPU, peak memory plus a buffer, minimum data points, the HPA gate that
returns `?`, and the optional OOMKill bump.

**Formatters** — `table`, `json`, `yaml`, `csv`, `csv-raw`, `html`, `pprint`. JSON and YAML
keep upstream's report shape.

**Discovery behaviour** — namespace values are treated as regular expressions and expanded
against the API; `kube-system` is skipped when no namespace filter is given; workload kinds and
label selectors filter the scan; jobs are listed page by page (`--discovery-job-batch-size`,
`--discovery-job-max-batches`); HPAs are read from `autoscaling/v2` with a fallback to `v1`;
one failing kind is logged and skipped rather than aborting the scan.

**History** — pods are found through kube-state-metrics, so pods deleted during the period are
still counted; long pod lists are split into batches of 50 per query; when several jobs export
the same series the `kubelet` one wins; when Prometheus knows no pods, the Kubernetes API is
used instead and the workload is flagged.

**Recommendations** — values are rounded up to whole millicores and mebibytes, floored at
`--cpu-min` and `--mem-min`, scored per workload and summed into the report's letter grade.

## Deliberately not ported

| Area | Upstream | Why |
| --- | --- | --- |
| Delivering the report to Slack, Azure Blob Storage, Teams or a Robusta runner | `--slackoutput`, `--slacktitle`, `--azurebloboutput`, `--teams-webhook`, `--azure-*`, `--publish_scan_url`, `--scan_id`, `--start_time`, `--named_sinks` | Out of scope for a local CLI: the report goes to stdout, or to a file with `--fileoutput` |
| AWS-signed access to EKS / Amazon Managed Prometheus | `--eks-*` (8 flags) | Needs request signing and an AWS credential chain; nothing else depends on it. Add it if you point KRR at AMP |
| Coralogix-hosted Prometheus | `--coralogix-token` | Same reason |
| Step-limit workaround for managed Prometheus | `core/runner.py` | Only reachable through the EKS flags above |
| Startup banner and update check | `utils/intro.py`, `utils/version.py` | Network calls on every run for an ASCII banner |
| Python client workarounds | `utils/patch.py`, `integrations/kubernetes/config_patch.py` | Patches for the python Kubernetes client with no equivalent here |
| In-cluster enforcement component | `enforcer/` | A separate deployment, not part of the CLI |

Flags for the dropped features are absent rather than silently ignored, so a command line
written for upstream fails loudly instead of quietly doing less.

## Deviations in behaviour

- **Prometheus client**: a thin client over `reqwest` replaces `prometrix`. Failed requests are
  retried with exponential backoff instead of a random 2-10 second wait.
- **Report order**: rows are sorted by cluster, namespace, workload and container; upstream
  prints them in discovery order, grouped by kind.
- **HTML output** is a standalone document built directly, not an export of the terminal table.
- **`ResourceAllocations.info`** always serializes both resource keys; upstream emits `{}` when
  the map is empty. The rest of the JSON report matches.
- **Expired job page tokens**: pagination for that namespace stops and keeps what was read;
  upstream retries with the token returned in the error body, which the Rust client does not
  expose.

## How parity is checked

- Resource formatting is pinned to values produced by upstream's `resource_units.py`, including
  its quirks (a value is only scaled up once the next unit reaches ten).
- The CSV formatter reproduces the golden rows of upstream's
  `tests/formatters/test_csv_formatter.py`.
- Percentiles interpolate exactly like `numpy.percentile` with its default method.
- The Prometheus client and loader run against a mock HTTP server: retries, pod batching, job
  de-duplication, pod discovery, history range and cluster summary.
- The Kubernetes layer runs against a mock API server: every workload kind, filters, the HPA
  fallback, job pagination and grouping, custom resources, and pod lookups.
