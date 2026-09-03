//! Pretty-printed debug dump of the whole report.
//!
//! Port of `robusta_krr/formatters/pprint.py`, which dumps the python objects;
//! the rust equivalent is the `Debug` representation.

use anyhow::Result;

use crate::result::ScanResult;

pub fn render(result: &ScanResult) -> Result<String> {
    Ok(format!("{result:#?}"))
}

#[cfg(test)]
mod tests {
    use super::super::tests::fixture;
    use super::*;

    #[test]
    fn dumps_every_field_of_the_report() {
        let output = render(&fixture()).unwrap();

        assert!(output.contains("ScanResult"), "{output}");
        assert!(output.contains("mock-object-1"), "{output}");
        assert!(output.contains("severity: Critical"), "{output}");
        assert!(output.contains("score:"), "{output}");
    }
}
