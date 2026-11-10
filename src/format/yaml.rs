//! YAML output. Port of `robusta_krr/formatters/yaml.py`.

use anyhow::Result;

use crate::result::ScanResult;

pub fn render(result: &ScanResult) -> Result<String> {
    Ok(yaml_serde::to_string(result)?)
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;

    #[test]
    fn round_trips_through_yaml() {
        let result = fixture();
        let parsed: ScanResult = yaml_serde::from_str(&render(&result).unwrap()).unwrap();
        assert_eq!(parsed, result);
    }

    /// `tests/fixtures/upstream_report.yaml` is upstream's formatter applied to this fixture:
    /// `yaml.dump(json.loads(<json report>), sort_keys=False)` with PyYAML.
    #[test]
    fn matches_upstream_byte_for_byte() {
        let expected = include_str!("../../tests/fixtures/upstream_report.yaml");
        assert_eq!(render(&fixture()).unwrap(), expected);
    }

    #[test]
    fn renders_readable_keys() {
        let rendered = render(&fixture()).unwrap();
        assert!(rendered.contains("severity: CRITICAL"), "{rendered}");
        assert!(rendered.contains("name: mock-object-1"), "{rendered}");
    }
}
