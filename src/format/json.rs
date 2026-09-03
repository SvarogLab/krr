//! JSON output. Port of `robusta_krr/formatters/json.py`.

use anyhow::Result;

use crate::result::ScanResult;

pub fn render(result: &ScanResult) -> Result<String> {
    Ok(serde_json::to_string_pretty(result)?)
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;

    #[test]
    fn round_trips_through_json() {
        let result = fixture();
        let rendered = render(&result).unwrap();
        let parsed: ScanResult = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed, result);
    }

    #[test]
    fn keeps_upstream_value_shapes() {
        let json: serde_json::Value = serde_json::from_str(&render(&fixture()).unwrap()).unwrap();
        assert_eq!(
            json["scans"][0]["recommended"]["limits"]["cpu"]["value"],
            serde_json::json!("?")
        );
        assert_eq!(
            json["scans"][0]["object"]["allocations"]["requests"]["cpu"],
            serde_json::json!(0.05)
        );
        assert_eq!(json["scans"][0]["severity"], serde_json::json!("CRITICAL"));
    }
}
