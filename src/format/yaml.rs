//! YAML output. Port of `robusta_krr/formatters/yaml.py`.

use anyhow::Result;

use crate::result::ScanResult;

pub fn render(result: &ScanResult) -> Result<String> {
    Ok(serde_yaml_ng::to_string(result)?)
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;

    #[test]
    fn round_trips_through_yaml() {
        let result = fixture();
        let parsed: ScanResult = serde_yaml_ng::from_str(&render(&result).unwrap()).unwrap();
        assert_eq!(parsed, result);
    }

    #[test]
    fn renders_readable_keys() {
        let rendered = render(&fixture()).unwrap();
        assert!(rendered.contains("severity: CRITICAL"), "{rendered}");
        assert!(rendered.contains("name: mock-object-1"), "{rendered}");
    }
}
