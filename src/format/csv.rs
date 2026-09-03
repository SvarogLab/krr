//! CSV output, with and without human readable formatting.
//!
//! Port of `robusta_krr/formatters/{csv,csv_raw}.py`.

use anyhow::Result;

use crate::model::{NAN_LITERAL, NONE_LITERAL, RecValue, ResourceType, Selector};
use crate::result::ScanResult;

use super::{Row, request_cell, rows, total_diff_cell};

/// Columns shared by both CSV flavours, in upstream order.
fn base_columns(result: &ScanResult) -> Vec<String> {
    let config = result.config();
    let mut columns: Vec<String> = Vec::new();

    if config.show_cluster_name {
        columns.push("Cluster".into());
    }
    columns
        .extend(["Namespace", "Name", "Pods", "Old Pods", "Type", "Container"].map(String::from));
    if config.show_severity {
        columns.push("Severity".into());
    }
    columns
}

fn base_cells(row: &Row, result: &ScanResult) -> Vec<String> {
    let config = result.config();
    let scan = row.scan;
    let object = &scan.object;
    // Identifying columns are only filled in on the first row of a workload group.
    let grouped = |value: String| if row.full_info { value } else { String::new() };

    let mut cells = Vec::new();
    if config.show_cluster_name {
        cells.push(object.cluster.clone().unwrap_or_default());
    }
    cells.push(grouped(object.namespace.clone()));
    cells.push(grouped(object.name.clone()));
    cells.push(grouped(object.current_pods_count().to_string()));
    cells.push(grouped(object.deleted_pods_count().to_string()));
    cells.push(grouped(object.kind.to_string()));
    cells.push(object.container.clone());
    if config.show_severity {
        cells.push(scan.severity.to_string());
    }
    cells
}

pub fn render(result: &ScanResult) -> Result<String> {
    let mut columns = base_columns(result);
    for resource in ResourceType::ALL {
        columns.push(format!("{} Diff", resource.name()));
        columns.push(format!("{} Requests", resource.name()));
        columns.push(format!("{} Limits", resource.name()));
    }

    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(&columns)?;

    for row in rows(result) {
        let mut cells = base_cells(&row, result);
        for resource in ResourceType::ALL {
            cells.push(total_diff_cell(row.scan, resource, false));
            cells.push(request_cell(row.scan, resource, Selector::Requests, false));
            cells.push(request_cell(row.scan, resource, Selector::Limits, false));
        }
        writer.write_record(&cells)?;
    }

    Ok(String::from_utf8(writer.into_inner()?)?)
}

/// Raw numbers instead of formatted quantities, one row per scan.
pub fn render_raw(result: &ScanResult) -> Result<String> {
    let mut columns = base_columns(result);
    for resource in ResourceType::ALL {
        for selector in ["Requests", "Limits"] {
            columns.push(format!("{} {selector} Current", resource.name()));
            columns.push(format!("{} {selector} Recommended", resource.name()));
        }
    }

    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record(&columns)?;

    for (index, scan) in result.scans.iter().enumerate() {
        // csv-raw repeats the identifying columns on every row
        let row = Row {
            number: index + 1,
            scan,
            full_info: true,
            last_in_group: true,
        };
        let mut cells = base_cells(&row, result);
        for resource in ResourceType::ALL {
            for selector in Selector::ALL {
                cells.push(raw_value(scan.object.allocations.get(selector)[resource]));
                cells.push(raw_value(scan.recommended.get(selector)[resource].value));
            }
        }
        writer.write_record(&cells)?;
    }

    Ok(String::from_utf8(writer.into_inner()?)?)
}

/// Numbers as-is: integers without a decimal point, `unset` and `?` unchanged.
fn raw_value(value: RecValue) -> String {
    match value {
        RecValue::Unset => NONE_LITERAL.to_string(),
        RecValue::Unknown => NAN_LITERAL.to_string(),
        RecValue::Val(value) if value.fract() == 0.0 => format!("{}", value as i64),
        RecValue::Val(value) => format!("{value}"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;

    fn parse(output: &str) -> (Vec<String>, Vec<Vec<String>>) {
        let mut reader = csv::Reader::from_reader(output.as_bytes());
        let headers = reader.headers().unwrap().iter().map(String::from).collect();
        let records = reader
            .records()
            .map(|r| r.unwrap().iter().map(String::from).collect())
            .collect();
        (headers, records)
    }

    fn cell(headers: &[String], row: &[String], name: &str) -> String {
        let index = headers
            .iter()
            .position(|h| h == name)
            .unwrap_or_else(|| panic!("no column {name}"));
        row[index].clone()
    }

    #[test]
    fn headers_match_upstream() {
        let (headers, _) = parse(&render(&fixture()).unwrap());
        assert_eq!(
            headers,
            vec![
                "Namespace",
                "Name",
                "Pods",
                "Old Pods",
                "Type",
                "Container",
                "Severity",
                "CPU Diff",
                "CPU Requests",
                "CPU Limits",
                "Memory Diff",
                "Memory Requests",
                "Memory Limits",
            ]
        );
    }

    #[test]
    fn severity_column_can_be_hidden() {
        let mut result = fixture();
        result.config.as_mut().unwrap().show_severity = false;
        let (headers, _) = parse(&render(&result).unwrap());
        assert!(!headers.contains(&"Severity".to_string()));
        assert_eq!(headers[0], "Namespace");
    }

    #[test]
    fn cluster_column_can_be_shown() {
        let mut result = fixture();
        result.config.as_mut().unwrap().show_cluster_name = true;
        let (headers, records) = parse(&render(&result).unwrap());
        assert_eq!(headers[0], "Cluster");
        assert_eq!(cell(&headers, &records[0], "Cluster"), "mock-cluster");
    }

    #[test]
    fn row_matches_upstream_golden_values() {
        let (headers, records) = parse(&render(&fixture()).unwrap());
        let row = &records[0];

        assert_eq!(cell(&headers, row, "Namespace"), "default");
        assert_eq!(cell(&headers, row, "Name"), "mock-object-1");
        assert_eq!(cell(&headers, row, "Pods"), "2");
        assert_eq!(cell(&headers, row, "Old Pods"), "1");
        assert_eq!(cell(&headers, row, "Type"), "Deployment");
        assert_eq!(cell(&headers, row, "Container"), "mock-container-1");
        assert_eq!(cell(&headers, row, "Severity"), "CRITICAL");
        assert_eq!(cell(&headers, row, "CPU Diff"), "-87m");
        assert_eq!(cell(&headers, row, "CPU Requests"), "(-43m) 50m -> 6m");
        assert_eq!(cell(&headers, row, "CPU Limits"), "2.0 -> ?");
        assert_eq!(cell(&headers, row, "Memory Diff"), "-4096Mi");
        assert_eq!(
            cell(&headers, row, "Memory Requests"),
            "(-2048Mi) 2048Mi -> 500m"
        );
        assert_eq!(cell(&headers, row, "Memory Limits"), "2.0 -> 500m");
    }

    #[test]
    fn raw_csv_reports_plain_numbers() {
        let (headers, records) = parse(&render_raw(&fixture()).unwrap());
        let row = &records[0];

        assert_eq!(cell(&headers, row, "CPU Requests Current"), "0.05");
        assert_eq!(cell(&headers, row, "CPU Requests Recommended"), "0.0065");
        assert_eq!(cell(&headers, row, "CPU Limits Current"), "2");
        assert_eq!(cell(&headers, row, "CPU Limits Recommended"), "?");
        assert_eq!(cell(&headers, row, "Memory Requests Current"), "2147483648");
        assert_eq!(cell(&headers, row, "Memory Requests Recommended"), "0.5");
    }

    #[test]
    fn repeated_workload_rows_are_blanked() {
        let mut result = fixture();
        let mut sidecar = result.scans[0].clone();
        sidecar.object.container = "sidecar".into();
        result.scans.push(sidecar);

        let (headers, records) = parse(&render(&result).unwrap());
        assert_eq!(cell(&headers, &records[1], "Namespace"), "");
        assert_eq!(cell(&headers, &records[1], "Name"), "");
        assert_eq!(cell(&headers, &records[1], "Container"), "sidecar");
    }
}
