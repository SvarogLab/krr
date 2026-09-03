# krr-rs

A Rust port of [robusta-dev/krr](https://github.com/robusta-dev/krr), the Kubernetes Resource
Recommender: it reads CPU and memory usage from Prometheus and tells you which requests and
limits your workloads actually need.

```sh
krr simple -p http://localhost:9090
```

Single static binary, no Python runtime, no cluster agent - KRR only reads.

## Install

```sh
cargo build --release      # target/release/krr
```

## Usage

```sh
krr simple                              # current kube context, auto-discovered Prometheus
krr simple -p http://localhost:9090     # explicit Prometheus (e.g. via kubectl port-forward)
krr simple -n prod -n 'staging-.*'      # namespaces, regular expressions allowed
krr simple -r Deployment -r StatefulSet # only these workload kinds
krr simple -f json --fileoutput out.json
krr simple_limit --cpu-request 66 --cpu-limit 96
krr simple --help                       # every flag
```

Run `krr simple --help` for the full list; flag names follow upstream (`-p`, `-n`, `-r`, `-s`,
`-f`, `-c/--context`, `--all-clusters`, `-H`, `-l`, `--cpu-min`, `--mem-min`, ...).

### Strategies

| Strategy | CPU request | CPU limit | Memory request/limit |
| --- | --- | --- | --- |
| `simple` (default) | 95th percentile of usage | unset | peak + 15 % |
| `simple_limit` | 66th percentile | 96th percentile | peak + 15 % |

Both look at 14 days of history at a 1.25 minute resolution by default, need 100 data points
before recommending anything, and return `?` for resources driven by an HPA (unless
`--allow-hpa`). `--use-oomkill-data` raises the memory recommendation above any limit that was
OOMKilled, plus `--oom-memory-buffer-percentage`.

### Output formats

`table` (default), `json`, `yaml`, `csv`, `csv-raw`, `html`. The JSON and YAML reports have the
same shape as upstream's, so existing consumers keep working.

## What it does

1. Reads the kubeconfig (or the in-cluster service account) and picks the contexts to scan.
2. Finds a metrics store: the `-p` URL, or a service in the cluster labelled like Victoria
   Metrics, Thanos, Mimir or Prometheus  -  reached directly from inside the cluster, or through
   the Kubernetes API server's service proxy from outside it.
3. Lists Deployments, StatefulSets, DaemonSets, Jobs, CronJobs and, when their CRDs are
   installed, Argo Rollouts, OpenShift DeploymentConfigs and Strimzi PodSets  -  one scannable
   object per container, together with the HPA that scales it.
4. Finds the pods each workload had during the period through kube-state-metrics, so pods that
   are already gone still count.
5. Loads the usage history per metric, batching large pod lists and preferring the `kubelet`
   job when several jobs export the same series.
6. Runs the strategy, rounds up to whole millicores / mebibytes, applies `--cpu-min` and
   `--mem-min`, scores every workload and prints the report.

## Architecture

| Module | Responsibility | Upstream counterpart |
| --- | --- | --- |
| `units` | resource quantities (`50m`, `2Gi`) | `utils/resource_units.py` |
| `model` | resources, allocations, workloads | `core/models/{allocations,objects}.py` |
| `severity` | how far off a recommendation is | `core/models/severity.py` |
| `result` | scans, score, diff rendering | `core/models/result.py` |
| `config` | settings shared by every layer | `core/models/config.py` |
| `format` | table/json/yaml/csv/html over one shared cell layer | `formatters/*` |
| `strategy` | `simple`, `simple_limit` and their shared math | `strategies/*` |
| `prometheus` | HTTP client, PromQL builders, loader, discovery | `integrations/prometheus/*` |
| `k8s` | workload, HPA and pod discovery | `integrations/kubernetes/*` |
| `runner` | orchestration, rounding, reporting | `core/runner.py` |
| `cli` | argument parsing and validation | `main.py` |

## Testing

```sh
cargo test
```

The port was written test-first. Where behaviour is observable, the tests pin it to upstream's:

- unit formatting is checked against values produced by upstream's own
  `resource_units.py` (including its quirks, such as `1024` printing unscaled);
- the CSV formatter reproduces the golden rows of upstream's
  `tests/formatters/test_csv_formatter.py`;
- percentiles interpolate exactly like `numpy.percentile`;
- the Prometheus client and loader are tested against a mock server (`wiremock`), including
  retries, pod batching, job de-duplication and pod discovery.

## Differences from upstream

Implemented differently:

- **Prometheus client**: a thin client over `reqwest` instead of `prometrix`. Requests retry
  with exponential backoff rather than a random 2-10 s wait.
- **HPA discovery** uses the `autoscaling/v2` API only; upstream falls back to `v1` on clusters
  older than 1.23.
- **HTML output** is a standalone document built directly, not a dump of the terminal table.
- `ResourceAllocations.info` always serializes both resource keys (upstream emits `{}` when
  empty). Everything else in the JSON report keeps upstream's shape.

Not ported (the flags are absent rather than silently ignored):

- authentication helpers for managed stores: AWS-signed EKS/AMP and Coralogix tokens
  (`--openshift` is supported and reads the service account token);
- publishing results to a Robusta runner, Slack, Azure Blob Storage or Teams;
- `GroupedJob` workloads (`--job-grouping-labels`) and the batched Job discovery flags;
- the `pprint` formatter (a Python `repr` dump) and the separate `enforcer` component.

## License

The upstream project is MIT licensed; this port follows it.
