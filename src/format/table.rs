//! Terminal table output. Port of `robusta_krr/formatters/table.py`.

use anyhow::Result;
use comfy_table::{ContentArrangement, Table, presets::UTF8_FULL};
use owo_colors::OwoColorize;

use crate::model::{ResourceType, Selector};
use crate::result::{ResourceScan, ScanResult};

use super::{paint, request_cell, rows, total_diff_cell};

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

    let mut table = Table::new();
    table
        .load_style(UTF8_FULL)
        .set_content_arrangement(ContentArrangement::Dynamic);
    if let Some(width) = config.width {
        table.set_width(width as u16);
    }
    table.set_header(
        headers
            .iter()
            .map(|header| format!("{}", header.magenta().bold())),
    );

    for row in rows(result) {
        let scan = row.scan;
        let object = &scan.object;
        let grouped = |value: String| if row.full_info { value } else { String::new() };

        let mut cells: Vec<String> = vec![paint(&format!("{}.", row.number), scan.severity, true)];
        if show_cluster {
            cells.push(grouped(object.cluster.clone().unwrap_or_default()));
        }
        cells.push(grouped(object.namespace.clone()));
        cells.push(grouped(object.name.clone()));
        cells.push(grouped(object.current_pods_count().to_string()));
        cells.push(grouped(object.deleted_pods_count().to_string()));
        cells.push(grouped(object.kind.to_string()));
        cells.push(object.container.clone());

        for resource in ResourceType::ALL {
            cells.push(total_diff_with_pods(scan, resource));
            for selector in Selector::ALL {
                cells.push(
                    request_cell(scan, resource, selector, true)
                        + &info_suffix(scan, resource, selector),
                );
            }
        }

        table.add_row(cells);
    }

    let mut output = String::new();
    if let Some(description) = &result.description {
        output.push_str(&format!("\n{description}\n\n"));
    }
    output.push_str(&table.to_string());
    output.push_str(&format!(
        "\n{} points - {}\n",
        result.score,
        result.score_letter()
    ));
    Ok(output)
}

/// The total diff plus a note about the pod count, which explains the multiplier.
fn total_diff_with_pods(scan: &ResourceScan, resource: ResourceType) -> String {
    let pods = scan.object.current_pods_count();
    let diff = total_diff_cell(scan, resource, true);
    if pods == 1 {
        diff
    } else {
        format!("{diff}\n({pods} pods)")
    }
}

/// Strategy notes such as "Not enough data", shown under the recommendation.
fn info_suffix(scan: &ResourceScan, resource: ResourceType, selector: Selector) -> String {
    if selector != Selector::Requests {
        // upstream renders the info once per resource, next to the requests column
        return String::new();
    }

    match scan.recommended.info[resource].as_deref() {
        None => String::new(),
        Some(info) if info == "OOMKill detected" => format!("\n{}", format!("({info})").red()),
        Some(info) => format!("\n{}", format!("({info})").dimmed()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;

    /// Strips ANSI escapes so assertions can look at the text only.
    fn plain(text: &str) -> String {
        let mut out = String::new();
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

    #[test]
    fn renders_headers_rows_and_score() {
        let output = plain(&render(&fixture()).unwrap());

        assert!(output.contains("Namespace"), "{output}");
        assert!(output.contains("CPU Requests"), "{output}");
        assert!(output.contains("mock-object-1"), "{output}");
        assert!(output.contains("(-43m) 50m -> 6m"), "{output}");
        assert!(
            output.contains("tests data"),
            "description is shown above the table"
        );
        assert!(output.contains("100 points - A"), "{output}");
    }

    #[test]
    fn shows_the_info_note_and_pod_count() {
        let output = plain(&render(&fixture()).unwrap());
        assert!(output.contains("(Not enough data)"), "{output}");
        assert!(output.contains("(2 pods)"), "{output}");
    }

    #[test]
    fn cluster_column_appears_for_multiple_clusters() {
        let result = fixture();
        assert!(!plain(&render(&result).unwrap()).contains("Cluster"));

        let mut multi = result.clone();
        let mut other = multi.scans[0].clone();
        other.object.cluster = Some("other-cluster".into());
        multi.scans.push(other);
        assert!(plain(&render(&multi).unwrap()).contains("Cluster"));
    }
}
