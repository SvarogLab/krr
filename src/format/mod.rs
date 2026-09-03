//! Output formatters and the cells they share.
//!
//! Port of `robusta_krr/core/abstract/formatters.py` and `robusta_krr/formatters/*`.

mod csv;
mod html;
mod json;
mod table;
mod yaml;

use anyhow::{Result, bail};

use crate::model::{NONE_LITERAL, RecValue, ResourceType, Selector, format_recommendation_value};
use crate::result::{ResourceScan, ScanResult, format_diff};
use crate::severity::Severity;

/// A formatter turns a scan result into text.
pub struct Formatter {
    pub name: &'static str,
    pub render: fn(&ScanResult) -> Result<String>,
    /// Whether the output carries ANSI colors (and must not be written to a file as-is).
    pub colored: bool,
}

pub const ALL: &[Formatter] = &[
    Formatter {
        name: "table",
        render: table::render,
        colored: true,
    },
    Formatter {
        name: "json",
        render: json::render,
        colored: false,
    },
    Formatter {
        name: "yaml",
        render: yaml::render,
        colored: false,
    },
    Formatter {
        name: "csv",
        render: csv::render,
        colored: false,
    },
    Formatter {
        name: "csv-raw",
        render: csv::render_raw,
        colored: false,
    },
    Formatter {
        name: "html",
        render: html::render,
        colored: false,
    },
];

pub fn find(name: &str) -> Result<&'static Formatter> {
    match ALL.iter().find(|formatter| formatter.name == name) {
        Some(formatter) => Ok(formatter),
        None => bail!(
            "Formatter '{name}' not found. Available formatters: {}",
            names().join(", ")
        ),
    }
}

pub fn names() -> Vec<&'static str> {
    ALL.iter().map(|formatter| formatter.name).collect()
}

/// One output line: a scan plus the grouping information the table and csv share.
pub struct Row<'a> {
    pub number: usize,
    pub scan: &'a ResourceScan,
    /// First row of a (cluster, namespace, name) group: identifying columns are only shown here.
    pub full_info: bool,
    /// Last row of a group, used to draw separators.
    pub last_in_group: bool,
}

/// Groups consecutive scans of the same workload, like `itertools.groupby` upstream.
pub fn rows(result: &ScanResult) -> Vec<Row<'_>> {
    let key = |scan: &ResourceScan| {
        (
            scan.object.cluster.clone(),
            scan.object.namespace.clone(),
            scan.object.name.clone(),
        )
    };

    let mut rows = Vec::with_capacity(result.scans.len());
    for (index, scan) in result.scans.iter().enumerate() {
        let previous = index.checked_sub(1).map(|i| key(&result.scans[i]));
        let next = result.scans.get(index + 1).map(key);
        rows.push(Row {
            number: index + 1,
            scan,
            full_info: previous.as_ref() != Some(&key(scan)),
            last_in_group: next.as_ref() != Some(&key(scan)),
        });
    }
    rows
}

/// `"(-43m) 50m -> 6m"`: the current value, the recommendation and the difference.
pub fn request_cell(
    scan: &ResourceScan,
    resource: ResourceType,
    selector: Selector,
    colored: bool,
) -> String {
    let allocated = scan.object.allocations.get(selector)[resource];
    let recommended = &scan.recommended.get(selector)[resource];

    if allocated == RecValue::Unset && recommended.value == RecValue::Unset {
        return paint(NONE_LITERAL, recommended.severity, colored);
    }

    let diff = format_diff(allocated, recommended.value, selector, 1.0, colored);
    let prefix = if diff.is_empty() {
        String::new()
    } else {
        format!("({diff}) ")
    };
    let body = format!(
        "{} -> {}",
        format_recommendation_value(allocated),
        format_recommendation_value(recommended.value)
    );

    prefix + &paint(&body, recommended.severity, colored)
}

/// The difference in requests across all running pods of the workload.
pub fn total_diff_cell(scan: &ResourceScan, resource: ResourceType, colored: bool) -> String {
    let pods = scan.object.current_pods_count();
    format_diff(
        scan.object.allocations.requests[resource],
        scan.recommended.requests[resource].value,
        Selector::Requests,
        pods as f64,
        colored,
    )
}

/// Colors text according to a severity, mirroring the rich colors upstream uses.
pub fn paint(text: &str, severity: Severity, colored: bool) -> String {
    if !colored {
        return text.to_string();
    }

    use owo_colors::OwoColorize;
    match severity {
        Severity::Unknown => format!("{}", text.dimmed()),
        Severity::Good => format!("{}", text.green()),
        Severity::Ok => format!("{}", text.bright_black()),
        Severity::Warning => format!("{}", text.yellow()),
        Severity::Critical => format!("{}", text.red()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The fixture mirrors upstream's `tests/formatters/test_csv_formatter.py` data.
    pub fn fixture() -> ScanResult {
        serde_json::from_str(include_str!("../../tests/fixtures/result.json"))
            .expect("valid fixture")
    }

    #[test]
    fn fixture_parses() {
        let result = fixture();
        assert_eq!(result.scans.len(), 1);
        assert_eq!(result.scans[0].object.current_pods_count(), 2);
        assert_eq!(result.scans[0].object.deleted_pods_count(), 1);
        assert_eq!(result.config().format, "csv");
        assert!(
            result.config().show_severity,
            "defaults apply to fields absent from the report"
        );
    }

    #[test]
    fn formatter_lookup() {
        assert_eq!(find("csv").unwrap().name, "csv");
        assert!(find("nope").is_err());
        assert!(names().contains(&"table"));
    }

    #[test]
    fn cells_match_upstream_golden_values() {
        let result = fixture();
        let scan = &result.scans[0];

        assert_eq!(
            request_cell(scan, ResourceType::Cpu, Selector::Requests, false),
            "(-43m) 50m -> 6m"
        );
        assert_eq!(
            request_cell(scan, ResourceType::Cpu, Selector::Limits, false),
            "2.0 -> ?"
        );
        assert_eq!(
            request_cell(scan, ResourceType::Memory, Selector::Requests, false),
            "(-2048Mi) 2048Mi -> 500m"
        );
        assert_eq!(
            request_cell(scan, ResourceType::Memory, Selector::Limits, false),
            "2.0 -> 500m"
        );
        assert_eq!(total_diff_cell(scan, ResourceType::Cpu, false), "-87m");
        assert_eq!(
            total_diff_cell(scan, ResourceType::Memory, false),
            "-4096Mi"
        );
    }

    #[test]
    fn rows_mark_the_first_of_each_workload() {
        let mut result = fixture();
        let mut second = result.scans[0].clone();
        second.object.container = "sidecar".into();
        let mut other = result.scans[0].clone();
        other.object.name = "other".into();
        result.scans.push(second);
        result.scans.push(other);

        let rows = rows(&result);
        let flags: Vec<(usize, bool, bool)> = rows
            .iter()
            .map(|row| (row.number, row.full_info, row.last_in_group))
            .collect();
        assert_eq!(
            flags,
            vec![(1, true, false), (2, false, true), (3, true, true)]
        );
    }
}
