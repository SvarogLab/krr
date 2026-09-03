//! Severity of a single recommendation.
//!
//! Port of `robusta_krr/core/models/severity.py`.

use serde::{Deserialize, Serialize};

use crate::model::{RecValue, ResourceType};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Severity {
    Unknown,
    Good,
    Ok,
    Warning,
    Critical,
}

impl Severity {
    /// Order used when picking the severity of a whole scan.
    pub const BY_PRIORITY: [Severity; 5] = [
        Severity::Critical,
        Severity::Warning,
        Severity::Ok,
        Severity::Good,
        Severity::Unknown,
    ];

    /// Calculates severity from the difference between current and recommended values.
    pub fn calculate(current: RecValue, recommended: RecValue, resource: ResourceType) -> Severity {
        let (current, recommended) = match (current, recommended) {
            (RecValue::Unknown, _) | (_, RecValue::Unknown) => return Severity::Unknown,
            (RecValue::Unset, RecValue::Unset) => return Severity::Good,
            (RecValue::Unset, _) | (_, RecValue::Unset) => return Severity::Warning,
            (RecValue::Val(current), RecValue::Val(recommended)) => (current, recommended),
        };

        let diff = (current - recommended).abs();
        let thresholds = match resource {
            // millicores
            ResourceType::Cpu => [0.5, 0.25, 0.1],
            // megabytes
            ResourceType::Memory => [500.0, 250.0, 100.0].map(|mb| mb * 1024.0 * 1024.0),
        };

        match diff {
            _ if diff >= thresholds[0] => Severity::Critical,
            _ if diff >= thresholds[1] => Severity::Warning,
            _ if diff >= thresholds[2] => Severity::Ok,
            _ => Severity::Good,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Unknown => "UNKNOWN",
            Severity::Good => "GOOD",
            Severity::Ok => "OK",
            Severity::Warning => "WARNING",
            Severity::Critical => "CRITICAL",
        }
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RecValue::{Unknown, Unset, Val};

    fn cpu(current: RecValue, recommended: RecValue) -> Severity {
        Severity::calculate(current, recommended, ResourceType::Cpu)
    }

    fn memory(current: RecValue, recommended: RecValue) -> Severity {
        Severity::calculate(current, recommended, ResourceType::Memory)
    }

    #[test]
    fn unknown_values_give_unknown_severity() {
        assert_eq!(cpu(Unknown, Val(1.0)), Severity::Unknown);
        assert_eq!(cpu(Val(1.0), Unknown), Severity::Unknown);
    }

    #[test]
    fn both_unset_is_good_one_unset_is_warning() {
        assert_eq!(cpu(Unset, Unset), Severity::Good);
        assert_eq!(cpu(Unset, Val(0.1)), Severity::Warning);
        assert_eq!(cpu(Val(0.1), Unset), Severity::Warning);
    }

    #[test]
    fn cpu_thresholds() {
        assert_eq!(cpu(Val(1.0), Val(1.05)), Severity::Good);
        assert_eq!(cpu(Val(1.0), Val(1.1)), Severity::Ok);
        assert_eq!(cpu(Val(1.0), Val(1.25)), Severity::Warning);
        assert_eq!(cpu(Val(1.0), Val(0.5)), Severity::Critical);
    }

    #[test]
    fn memory_thresholds_are_in_megabytes() {
        let mb = 1024.0 * 1024.0;
        assert_eq!(memory(Val(0.0), Val(50.0 * mb)), Severity::Good);
        assert_eq!(memory(Val(0.0), Val(100.0 * mb)), Severity::Ok);
        assert_eq!(memory(Val(0.0), Val(250.0 * mb)), Severity::Warning);
        assert_eq!(memory(Val(0.0), Val(500.0 * mb)), Severity::Critical);
    }

    #[test]
    fn serializes_uppercase() {
        assert_eq!(
            serde_json::to_string(&Severity::Critical).unwrap(),
            r#""CRITICAL""#
        );
        assert_eq!(Severity::Critical.to_string(), "CRITICAL");
    }
}
