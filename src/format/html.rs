//! Standalone HTML report. Upstream exports the rich table; here the table is built directly.

use anyhow::Result;

use crate::model::{ResourceType, Selector};
use crate::result::ScanResult;
use crate::severity::Severity;

use super::{request_cell, rows, total_diff_cell};

const STYLE: &str = "\
body { font-family: system-ui, sans-serif; margin: 2rem; }
table { border-collapse: collapse; }
th, td { border: 1px solid #d0d0d0; padding: 4px 8px; text-align: left; white-space: nowrap; }
th { background: #f4f4f4; }
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

    let mut html = String::new();
    html.push_str("<!doctype html>\n<html>\n<head>\n<meta charset=\"utf-8\">\n");
    html.push_str("<title>KRR scan</title>\n<style>\n");
    html.push_str(STYLE);
    html.push_str("\n</style>\n</head>\n<body>\n");

    if let Some(description) = &result.description {
        html.push_str(&format!(
            "<p>{}</p>\n",
            escape(description).replace('\n', "<br>")
        ));
    }

    html.push_str("<table>\n<thead>\n<tr>");
    for header in &headers {
        html.push_str(&format!("<th>{}</th>", escape(header)));
    }
    html.push_str("</tr>\n</thead>\n<tbody>\n");

    for row in rows(result) {
        let scan = row.scan;
        let object = &scan.object;
        let grouped = |value: String| if row.full_info { value } else { String::new() };

        let mut cells: Vec<(String, Option<Severity>)> =
            vec![(format!("{}.", row.number), Some(scan.severity))];
        if show_cluster {
            cells.push((grouped(object.cluster.clone().unwrap_or_default()), None));
        }
        cells.push((grouped(object.namespace.clone()), None));
        cells.push((grouped(object.name.clone()), None));
        cells.push((grouped(object.current_pods_count().to_string()), None));
        cells.push((grouped(object.deleted_pods_count().to_string()), None));
        cells.push((grouped(object.kind.to_string()), None));
        cells.push((object.container.clone(), None));

        for resource in ResourceType::ALL {
            cells.push((total_diff_cell(scan, resource, false), None));
            for selector in Selector::ALL {
                let severity = scan.recommended.get(selector)[resource].severity;
                let mut text = request_cell(scan, resource, selector, false);
                if selector == Selector::Requests
                    && let Some(info) = scan.recommended.info[resource].as_deref()
                {
                    text.push_str(&format!(
                        "</span><br><span class=\"info\">({})",
                        escape(info)
                    ));
                }
                cells.push((text, Some(severity)));
            }
        }

        html.push_str("<tr>");
        for (text, severity) in cells {
            match severity {
                Some(severity) => html.push_str(&format!(
                    "<td><span class=\"{}\">{}</span></td>",
                    severity,
                    escape_keeping_spans(&text)
                )),
                None => html.push_str(&format!("<td>{}</td>", escape(&text))),
            }
        }
        html.push_str("</tr>\n");
    }

    html.push_str("</tbody>\n</table>\n");
    html.push_str(&format!(
        "<p>{} points - {}</p>\n",
        result.score,
        result.score_letter()
    ));
    html.push_str("</body>\n</html>\n");
    Ok(html)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escapes user text but keeps the `<span>`/`<br>` markup this module inserts itself.
fn escape_keeping_spans(text: &str) -> String {
    escape(text)
        .replace(
            "&lt;/span&gt;&lt;br&gt;&lt;span class=&quot;info&quot;&gt;",
            "</span><br><span class=\"info\">",
        )
        .replace(
            "&lt;/span&gt;&lt;br&gt;&lt;span class=\"info\"&gt;",
            "</span><br><span class=\"info\">",
        )
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;

    #[test]
    fn renders_a_standalone_document() {
        let html = render(&fixture()).unwrap();
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("<table>"));
        assert!(html.ends_with("</html>\n"));
    }

    #[test]
    fn renders_values_and_severity_classes() {
        let html = render(&fixture()).unwrap();
        assert!(html.contains("mock-object-1"), "{html}");
        assert!(html.contains("(-43m) 50m -&gt; 6m"), "{html}");
        assert!(html.contains(r#"class="CRITICAL""#), "{html}");
        assert!(html.contains("(Not enough data)"), "{html}");
        assert!(html.contains("100 points - A"), "{html}");
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
