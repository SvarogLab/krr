//! Tests of the Kubernetes discovery layer against a mock API server.
//!
//! These cover the paths that need a real API shape: pagination, the HPA
//! fallback, custom resources and pod lookups.

use krr::config::{Config, Filter};
use krr::k8s::ClusterLoader;
use krr::model::{K8sObject, Kind, ResourceType};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn loader(server: &MockServer, config: Config) -> ClusterLoader {
    let kube_config = kube::Config::new(server.uri().parse().expect("valid uri"));
    let client = kube::Client::try_from(kube_config).expect("client");
    ClusterLoader::new(client, None, config)
}

/// An API list response.
fn list(items: Vec<Value>) -> Value {
    json!({"apiVersion": "v1", "kind": "List", "metadata": {"resourceVersion": "1"}, "items": items})
}

/// A list response that tells the client to ask for another page.
fn page(items: Vec<Value>, continue_token: &str) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "List",
        "metadata": {"resourceVersion": "1", "continue": continue_token},
        "items": items,
    })
}

fn container(name: &str, cpu: &str, memory: &str) -> Value {
    json!({"name": name, "resources": {"requests": {"cpu": cpu, "memory": memory}}})
}

fn workload(name: &str, namespace: &str, containers: Vec<Value>) -> Value {
    json!({
        "metadata": {"name": name, "namespace": namespace, "uid": format!("uid-{name}")},
        "spec": {
            "selector": {"matchLabels": {"app": name}},
            "template": {"spec": {"containers": containers}},
        },
    })
}

fn cron_job(name: &str, namespace: &str) -> Value {
    json!({
        "metadata": {"name": name, "namespace": namespace, "uid": format!("uid-{name}")},
        "spec": {
            "schedule": "* * * * *",
            "jobTemplate": {"spec": {"template": {"spec": {"containers": [container("app", "10m", "64Mi")]}}}},
        },
    })
}

fn job(name: &str, namespace: &str, labels: Value, owner: Option<&str>) -> Value {
    let mut metadata = json!({"name": name, "namespace": namespace, "uid": format!("uid-{name}"), "labels": labels});
    if let Some(kind) = owner {
        metadata["ownerReferences"] =
            json!([{"apiVersion": "batch/v1", "kind": kind, "name": "owner", "uid": "owner-uid"}]);
    }
    json!({
        "metadata": metadata,
        "spec": {"template": {"spec": {"containers": [container("app", "10m", "64Mi")]}}},
    })
}

/// Serves an empty list for everything, so a test only mocks what it cares about.
async fn serve_empty(server: &MockServer) {
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list(vec![])))
        .mount(server)
        .await;
}

async fn serve(server: &MockServer, api_path: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(api_path.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn names(objects: &[K8sObject]) -> Vec<String> {
    let mut names: Vec<String> = objects
        .iter()
        .map(|o| format!("{}/{}/{}", o.kind, o.namespace, o.name))
        .collect();
    names.sort();
    names
}

#[tokio::test]
async fn lists_every_builtin_workload_kind() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/apis/apps/v1/deployments",
        list(vec![workload(
            "web",
            "default",
            vec![container("app", "50m", "1Gi")],
        )]),
    )
    .await;
    serve(
        &server,
        "/apis/apps/v1/statefulsets",
        list(vec![workload(
            "db",
            "default",
            vec![container("db", "1", "2Gi")],
        )]),
    )
    .await;
    serve(
        &server,
        "/apis/apps/v1/daemonsets",
        list(vec![workload(
            "agent",
            "default",
            vec![container("agent", "10m", "64Mi")],
        )]),
    )
    .await;
    serve(
        &server,
        "/apis/batch/v1/jobs",
        list(vec![job("import", "default", json!({}), None)]),
    )
    .await;
    serve(
        &server,
        "/apis/batch/v1/cronjobs",
        list(vec![cron_job("cleanup", "default")]),
    )
    .await;
    serve_empty(&server).await;

    let objects = loader(&server, Config::default())
        .list_scannable_objects()
        .await
        .unwrap();

    assert_eq!(
        names(&objects),
        vec![
            "CronJob/default/cleanup",
            "DaemonSet/default/agent",
            "Deployment/default/web",
            "Job/default/import",
            "StatefulSet/default/db",
        ]
    );

    let web = objects.iter().find(|o| o.name == "web").unwrap();
    assert_eq!(web.container, "app");
    assert_eq!(
        web.allocations.requests[ResourceType::Cpu].as_f64(),
        Some(0.05)
    );
    assert_eq!(web.pod_selector.as_deref(), Some("app=web"));
    assert_eq!(web.uid.as_deref(), Some("uid-web"));
}

#[tokio::test]
async fn kube_system_is_skipped_unless_asked_for() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/apis/apps/v1/deployments",
        list(vec![
            workload("web", "default", vec![container("app", "50m", "1Gi")]),
            workload("dns", "kube-system", vec![container("app", "50m", "1Gi")]),
        ]),
    )
    .await;
    serve_empty(&server).await;

    let objects = loader(&server, Config::default())
        .list_scannable_objects()
        .await
        .unwrap();
    assert_eq!(names(&objects), vec!["Deployment/default/web"]);
}

#[tokio::test]
async fn a_namespace_filter_queries_only_those_namespaces() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/apis/apps/v1/namespaces/prod/deployments",
        list(vec![workload(
            "web",
            "prod",
            vec![container("app", "50m", "1Gi")],
        )]),
    )
    .await;
    serve_empty(&server).await;

    let config = Config {
        namespaces: Filter::Only(vec!["prod".into()]),
        ..Default::default()
    };
    let objects = loader(&server, config)
        .list_scannable_objects()
        .await
        .unwrap();
    assert_eq!(names(&objects), vec!["Deployment/prod/web"]);
}

#[tokio::test]
async fn a_resource_filter_skips_other_kinds() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/apis/apps/v1/deployments",
        list(vec![workload(
            "web",
            "default",
            vec![container("app", "50m", "1Gi")],
        )]),
    )
    .await;
    serve(
        &server,
        "/apis/batch/v1/jobs",
        list(vec![job("import", "default", json!({}), None)]),
    )
    .await;
    serve_empty(&server).await;

    let config = Config {
        resources: Filter::Only(vec![Kind::Deployment]),
        ..Default::default()
    };
    let objects = loader(&server, config)
        .list_scannable_objects()
        .await
        .unwrap();
    assert_eq!(names(&objects), vec!["Deployment/default/web"]);
}

#[tokio::test]
async fn workloads_are_matched_with_their_autoscaler() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/apis/apps/v1/deployments",
        list(vec![workload(
            "web",
            "default",
            vec![container("app", "50m", "1Gi")],
        )]),
    )
    .await;
    serve(
        &server,
        "/apis/autoscaling/v2/horizontalpodautoscalers",
        list(vec![json!({
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {
                "maxReplicas": 10,
                "minReplicas": 2,
                "scaleTargetRef": {"kind": "Deployment", "name": "web"},
                "metrics": [{"type": "Resource", "resource": {"name": "cpu", "target": {"type": "Utilization", "averageUtilization": 80}}}],
            },
            "status": {"currentReplicas": 3, "desiredReplicas": 4},
        })]),
    )
    .await;
    serve_empty(&server).await;

    let objects = loader(&server, Config::default())
        .list_scannable_objects()
        .await
        .unwrap();
    let hpa = objects[0].hpa.as_ref().expect("hpa was matched");
    assert_eq!(hpa.target_cpu_utilization_percentage, Some(80.0));
    assert_eq!(hpa.max_replicas, 10);
    assert_eq!(hpa.current_replicas, Some(3));
}

#[tokio::test]
async fn older_clusters_fall_back_to_autoscaling_v1() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/apis/apps/v1/deployments",
        list(vec![workload(
            "web",
            "default",
            vec![container("app", "50m", "1Gi")],
        )]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/apis/autoscaling/v2/horizontalpodautoscalers"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "kind": "Status", "status": "Failure", "message": "404 page not found", "reason": "NotFound", "code": 404
        })))
        .mount(&server)
        .await;
    serve(
        &server,
        "/apis/autoscaling/v1/horizontalpodautoscalers",
        list(vec![json!({
            "metadata": {"name": "web", "namespace": "default"},
            "spec": {
                "maxReplicas": 5,
                "scaleTargetRef": {"kind": "Deployment", "name": "web"},
                "targetCPUUtilizationPercentage": 70,
            },
            "status": {"currentReplicas": 1, "desiredReplicas": 2},
        })]),
    )
    .await;
    serve_empty(&server).await;

    let objects = loader(&server, Config::default())
        .list_scannable_objects()
        .await
        .unwrap();
    let hpa = objects[0].hpa.as_ref().expect("hpa was matched through v1");
    assert_eq!(hpa.target_cpu_utilization_percentage, Some(70.0));
    assert_eq!(hpa.target_memory_utilization_percentage, None);
    assert_eq!(hpa.max_replicas, 5);
}

#[tokio::test]
async fn jobs_are_read_page_by_page() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/apis/batch/v1/jobs"))
        .and(query_param("continue", "page-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list(vec![job(
            "second",
            "default",
            json!({}),
            None,
        )])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/apis/batch/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![job("first", "default", json!({}), None)],
            "page-2",
        )))
        .mount(&server)
        .await;
    serve_empty(&server).await;

    let config = Config {
        discovery_job_batch_size: 1,
        ..Default::default()
    };
    let objects = loader(&server, config)
        .list_scannable_objects()
        .await
        .unwrap();

    assert_eq!(
        names(&objects),
        vec!["Job/default/first", "Job/default/second"]
    );
}

#[tokio::test]
async fn job_pages_are_capped() {
    let server = MockServer::start().await;
    // every page points at another one: without a cap this would never end
    Mock::given(method("GET"))
        .and(path("/apis/batch/v1/jobs"))
        .respond_with(ResponseTemplate::new(200).set_body_json(page(
            vec![job("endless", "default", json!({}), None)],
            "next",
        )))
        .mount(&server)
        .await;
    serve_empty(&server).await;

    let config = Config {
        discovery_job_batch_size: 1,
        discovery_job_max_batches: 3,
        ..Default::default()
    };
    let objects = loader(&server, config)
        .list_scannable_objects()
        .await
        .unwrap();

    assert_eq!(
        objects.len(),
        3,
        "one object per page, capped at three pages"
    );
}

#[tokio::test]
async fn jobs_sharing_a_label_become_one_grouped_workload() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/apis/batch/v1/jobs",
        list(vec![
            job("batch-1", "default", json!({"app": "etl"}), None),
            job("batch-2", "default", json!({"app": "etl"}), None),
            job("plain", "default", json!({}), None),
            job(
                "from-cron",
                "default",
                json!({"app": "etl"}),
                Some("CronJob"),
            ),
        ]),
    )
    .await;
    serve_empty(&server).await;

    let config = Config {
        job_grouping_labels: Some(vec!["app".into()]),
        job_grouping_limit: 500,
        ..Default::default()
    };
    let objects = loader(&server, config)
        .list_scannable_objects()
        .await
        .unwrap();

    // the grouped jobs are reported as one workload, the ungrouped one on its own
    assert_eq!(
        names(&objects),
        vec!["GroupedJob/default/app=etl", "Job/default/plain"]
    );

    let group = objects.iter().find(|o| o.kind == Kind::GroupedJob).unwrap();
    assert_eq!(group.pod_selector.as_deref(), Some("app=etl"));
    assert_eq!(group.grouped_jobs, vec!["batch-1", "batch-2"]);
}

#[tokio::test]
async fn missing_custom_resource_definitions_are_not_an_error() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/apis/apps/v1/deployments",
        list(vec![workload(
            "web",
            "default",
            vec![container("app", "50m", "1Gi")],
        )]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/apis/argoproj.io/v1alpha1/rollouts"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    serve_empty(&server).await;

    let objects = loader(&server, Config::default())
        .list_scannable_objects()
        .await
        .unwrap();
    assert_eq!(names(&objects), vec!["Deployment/default/web"]);
}

#[tokio::test]
async fn custom_resources_are_scanned_when_installed() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/apis/argoproj.io/v1alpha1/rollouts",
        list(vec![json!({
            "metadata": {"name": "canary", "namespace": "default"},
            "spec": {
                "selector": {"matchLabels": {"app": "canary"}},
                "template": {"spec": {"containers": [container("app", "100m", "256Mi")]}},
            },
        })]),
    )
    .await;
    serve_empty(&server).await;

    let objects = loader(&server, Config::default())
        .list_scannable_objects()
        .await
        .unwrap();
    assert_eq!(names(&objects), vec!["Rollout/default/canary"]);
    assert_eq!(
        objects[0].allocations.requests[ResourceType::Cpu].as_f64(),
        Some(0.1)
    );
    assert_eq!(objects[0].pod_selector.as_deref(), Some("app=canary"));
}

#[tokio::test]
async fn pods_are_listed_by_the_workload_selector() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/apis/apps/v1/deployments",
        list(vec![workload(
            "web",
            "default",
            vec![container("app", "50m", "1Gi")],
        )]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/namespaces/default/pods"))
        .and(query_param("labelSelector", "app=web"))
        .respond_with(ResponseTemplate::new(200).set_body_json(list(vec![
            json!({"metadata": {"name": "web-1", "namespace": "default"}}),
            json!({"metadata": {"name": "web-2", "namespace": "default"}}),
        ])))
        .mount(&server)
        .await;
    serve_empty(&server).await;

    let loader = loader(&server, Config::default());
    let objects = loader.list_scannable_objects().await.unwrap();
    let pods = loader.list_pods(&objects[0]).await.unwrap();

    assert_eq!(
        pods.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["web-1", "web-2"]
    );
    assert!(pods.iter().all(|pod| !pod.deleted));
}

#[tokio::test]
async fn cron_job_pods_are_found_through_the_jobs_it_owns() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/apis/batch/v1/cronjobs",
        list(vec![cron_job("cleanup", "default")]),
    )
    .await;
    serve(
        &server,
        "/apis/batch/v1/namespaces/default/jobs",
        list(vec![
            json!({
                "metadata": {
                    "name": "cleanup-1", "namespace": "default", "uid": "job-uid-1",
                    "ownerReferences": [{"apiVersion": "batch/v1", "kind": "CronJob", "name": "cleanup", "uid": "uid-cleanup"}],
                },
                "spec": {"template": {"spec": {"containers": [container("app", "10m", "64Mi")]}}},
            }),
            job("unrelated", "default", json!({}), None),
        ]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/namespaces/default/pods"))
        .and(query_param(
            "labelSelector",
            "batch.kubernetes.io/controller-uid in (job-uid-1)",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(list(vec![
            json!({"metadata": {"name": "cleanup-1-abc", "namespace": "default"}}),
        ])))
        .mount(&server)
        .await;
    serve_empty(&server).await;

    let loader = loader(&server, Config::default());
    let objects = loader.list_scannable_objects().await.unwrap();
    let cron = objects.iter().find(|o| o.kind == Kind::CronJob).unwrap();

    let pods = loader.list_pods(cron).await.unwrap();
    assert_eq!(
        pods.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        vec!["cleanup-1-abc"]
    );
}

#[tokio::test]
async fn namespace_patterns_are_expanded_against_the_api() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/api/v1/namespaces",
        list(vec![
            json!({"metadata": {"name": "prod-eu"}}),
            json!({"metadata": {"name": "prod-us"}}),
            json!({"metadata": {"name": "staging"}}),
        ]),
    )
    .await;
    serve(
        &server,
        "/apis/apps/v1/namespaces/prod-eu/deployments",
        list(vec![workload(
            "web",
            "prod-eu",
            vec![container("app", "50m", "1Gi")],
        )]),
    )
    .await;
    serve(
        &server,
        "/apis/apps/v1/namespaces/prod-us/deployments",
        list(vec![workload(
            "web",
            "prod-us",
            vec![container("app", "50m", "1Gi")],
        )]),
    )
    .await;
    serve_empty(&server).await;

    let config = Config {
        namespaces: Filter::Only(vec!["prod-.*".into()]),
        ..Default::default()
    };
    let objects = loader(&server, config)
        .list_scannable_objects()
        .await
        .unwrap();

    assert_eq!(
        names(&objects),
        vec!["Deployment/prod-eu/web", "Deployment/prod-us/web"]
    );
}

#[tokio::test]
async fn a_failing_kind_does_not_abort_the_scan() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/apis/apps/v1/deployments",
        list(vec![workload(
            "web",
            "default",
            vec![container("app", "50m", "1Gi")],
        )]),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/apis/apps/v1/statefulsets"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    serve_empty(&server).await;

    let objects = loader(&server, Config::default())
        .list_scannable_objects()
        .await
        .unwrap();
    assert_eq!(names(&objects), vec!["Deployment/default/web"]);
}
