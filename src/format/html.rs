//! Standalone HTML report. Upstream exports the rich table; here the table is built directly.

use anyhow::Result;
use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::model::{ResourceType, Selector};
use crate::result::ScanResult;

use super::{request_cell, rows, total_diff_cell, total_lines};

const STYLE: &str = "\
body { font-family: system-ui, sans-serif; margin: 2rem; }
table { border-collapse: collapse; }
th, td { border: 1px solid #d0d0d0; padding: 4px 8px; text-align: left; white-space: nowrap; }
th { background: #f4f4f4; }
tfoot td, tfoot th { background: #fafafa; text-align: left; }
.UNKNOWN { color: #888; }
.GOOD { color: #1a7f37; }
.OK { color: #555; }
.WARNING { color: #9a6700; }
.CRITICAL { color: #cf222e; }
.info { color: #888; font-size: 0.85em; }";

pub fn render(result: &ScanResult) -> Result<String> {
    let config = result.config();
    let clusters: std::collections::BTreeSet<_> = result
        .scans
        .iter()
        .map(|scan| &scan.object.cluster)
        .collect();
    let show_cluster = clusters.len() > 1 || config.show_cluster_name;

    let mut headers: Vec<String> = vec!["Number".into()];
    if show_cluster {
        headers.push("Cluster".into());
    }
    headers
        .extend(["Namespace", "Name", "Pods", "Old Pods", "Type", "Container"].map(String::from));
    for resource in ResourceType::ALL {
        headers.push(format!("{} Diff", resource.name()));
        headers.push(format!("{} Requests", resource.name()));
        headers.push(format!("{} Limits", resource.name()));
    }

    let (totals, total_lines) = total_lines(result, false);

    let markup = html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                title { "KRR scan" }
                style { (PreEscaped(STYLE)) }
            }
            body {
                @if let Some(description) = &result.description {
                    p {
                        @for (index, line) in description.lines().enumerate() {
                            @if index > 0 { br; }
                            (line)
                        }
                    }
                }

                table {
                    thead {
                        tr { @for header in &headers { th { (header) } } }
                    }
                    tbody {
                        @for row in rows(result) {
                            @let scan = row.scan;
                            @let object = &scan.object;
                            // identifying columns are only filled in on the first row of a group
                            @let grouped = |value: String| if row.full_info { value } else { String::new() };
                            tr {
                                td { span class=(scan.severity) { (row.number) "." } }
                                @if show_cluster {
                                    td { (grouped(object.cluster.clone().unwrap_or_default())) }
                                }
                                td { (grouped(object.namespace.clone())) }
                                td { (grouped(object.name.clone())) }
                                td { (grouped(object.current_pods_count().to_string())) }
                                td { (grouped(object.deleted_pods_count().to_string())) }
                                td { (grouped(object.kind.to_string())) }
                                td { (object.container) }

                                @for resource in ResourceType::ALL {
                                    td { (total_diff_cell(scan, resource, false)) }
                                    @for selector in Selector::ALL {
                                        @let severity = scan.recommended.get(selector)[resource].severity;
                                        td {
                                            span class=(severity) {
                                                (request_cell(scan, resource, selector, false))
                                            }
                                            @if selector == Selector::Requests
                                                && let Some(info) = scan.recommended.info[resource].as_deref()
                                            {
                                                br;
                                                span class="info" { "(" (info) ")" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    tfoot {
                        tr {
                            th colspan=(headers.len()) {
                                "Totals for " (totals.workloads)
                                @if totals.workloads == 1 { " workload" } @else { " workloads" }
                                " (requests of all running pods)"
                            }
                        }
                        @for line in total_lines {
                            tr {
                                td colspan=(headers.len()) {
                                    (line.resource.name()) ": " (line.current)
                                    " -> " (line.recommended) " (" (line.diff) ")"
                                }
                            }
                        }
                    }
                }

                p { (result.score) " points - " (result.score_letter()) }
            }
        }
    };

    Ok(finish(markup))
}

/// maud renders one long line; a trailing newline keeps the file well formed.
fn finish(markup: Markup) -> String {
    let mut html = markup.into_string();
    html.push('\n');
    html
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;

    #[test]
    fn renders_a_standalone_document() {
        let html = render(&fixture()).unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<table>"));
        assert!(html.trim_end().ends_with("</html>"));
    }

    #[test]
    fn renders_values_and_severity_classes() {
        let html = render(&fixture()).unwrap();
        assert!(html.contains("mock-object-1"), "{html}");
        assert!(html.contains("(-43m) 50m -&gt; 6m"), "{html}");
        assert!(html.contains(r#"class="CRITICAL""#), "{html}");
        assert!(html.contains("(Not enough data)"), "{html}");
        assert!(html.contains("100 points - A"), "{html}");
        assert!(html.contains("Totals for 1 workload"), "{html}");
        assert!(html.contains("CPU: 100m -&gt; 13m (-87m)"), "{html}");
    }

    #[test]
    fn escapes_workload_names() {
        let mut result = fixture();
        result.scans[0].object.name = "<script>alert(1)</script>".into();
        let html = render(&result).unwrap();
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("&lt;script&gt;"), "{html}");
    }
}
