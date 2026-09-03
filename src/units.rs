//! Parsing and formatting of Kubernetes resource quantities.
//!
//! Port of `robusta_krr/utils/resource_units.py`.

/// Suffixes ordered so that binary units ("Mi") are matched before decimal ones ("M").
const UNITS: &[(&str, f64)] = &[
    ("m", 1e-3),
    ("Ki", 1024.0),
    ("Mi", 1024f64 * 1024.0),
    ("Gi", 1024f64 * 1024.0 * 1024.0),
    ("Ti", 1024f64 * 1024.0 * 1024.0 * 1024.0),
    ("Pi", 1024f64 * 1024.0 * 1024.0 * 1024.0 * 1024.0),
    ("Ei", 1024f64 * 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0),
    ("k", 1e3),
    ("M", 1e6),
    ("G", 1e9),
    ("T", 1e12),
    ("P", 1e15),
    ("E", 1e18),
];

const DECIMAL_UNITS: &[&str] = &["", "K", "M", "G", "T", "P", "E"];
const BINARY_UNITS: &[&str] = &["", "Ki", "Mi", "Gi", "Ti", "Pi", "Ei"];

/// Converts a quantity string ("128Mi", "5m", "1.5") into a number.
pub fn parse(value: &str) -> Result<f64, std::num::ParseFloatError> {
    for (unit, multiplier) in UNITS {
        if let Some(head) = value.strip_suffix(unit) {
            return Ok(head.parse::<f64>()? * multiplier);
        }
    }
    value.parse::<f64>()
}

/// Converts a number into a quantity string, mirroring `resource_units.format`.
pub fn format(value: f64) -> String {
    format_with_base(value, 1024.0)
}

/// Same as [`format`] but with an explicit base (1024 or 1000).
pub fn format_with_base(value: f64, base: f64) -> String {
    if value < 1.0 {
        return std::format!("{}m", (value * 1000.0).trunc() as i64);
    }
    if value < base {
        return python_float(value);
    }

    let units = if base == 1024.0 {
        BINARY_UNITS
    } else {
        DECIMAL_UNITS
    };
    let value = value.trunc();
    for (i, unit) in units.iter().enumerate() {
        let scale = base.powi(i as i32 + 1);
        if value < scale || i == units.len() - 1 || value / scale < 10.0 {
            return std::format!("{:.0}{}", value / base.powi(i as i32), unit);
        }
    }
    unreachable!("the last iteration always returns")
}

/// Renders a float the way `str()` does in python: integral values keep one decimal.
fn python_float(value: f64) -> String {
    if value.fract() == 0.0 {
        std::format!("{:.1}", value)
    } else {
        std::format!("{}", value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_numbers() {
        assert_eq!(parse("0.005").unwrap(), 0.005);
        assert_eq!(parse("128974848e0").unwrap(), 128974848.0);
    }

    #[test]
    fn parses_decimal_and_binary_suffixes() {
        assert_eq!(parse("5m").unwrap(), 0.005);
        // python yields 128974848.00000001 here too: keep the same float math
        assert!((parse("128.9748480M").unwrap() - 128974848.0).abs() < 1e-6);
        assert_eq!(parse("128974848000m").unwrap(), 128974848.0);
        assert_eq!(parse("123Mi").unwrap(), 123.0 * 1024.0 * 1024.0);
        assert_eq!(parse("2048Mi").unwrap(), 2147483648.0);
        assert_eq!(parse("1k").unwrap(), 1000.0);
        assert_eq!(parse("1Ki").unwrap(), 1024.0);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("not-a-number").is_err());
    }

    #[test]
    fn formats_sub_unit_values_as_millis() {
        // int() in python truncates towards zero
        assert_eq!(format(0.0065), "6m");
        assert_eq!(format(0.5), "500m");
        assert_eq!(format(0.05), "50m");
        assert_eq!(format(0.0), "0m");
    }

    #[test]
    fn formats_small_values_like_python_floats() {
        assert_eq!(format(2.0), "2.0");
        assert_eq!(format(1.5), "1.5");
        assert_eq!(format(512.0), "512.0");
    }

    #[test]
    fn formats_large_values_with_binary_units() {
        assert_eq!(format(2147483648.0), "2048Mi");
        assert_eq!(format(2147483647.5), "2048Mi");
        assert_eq!(format(4294967295.0), "4096Mi");
        // upstream quirk: a value is only scaled up once the next unit reaches 10
        assert_eq!(format(1024.0), "1024");
        assert_eq!(format(1073741824.0), "1024Mi");
        assert_eq!(format(123.0 * 1024.0 * 1024.0), "123Mi");
        // scales up only once the next unit is >= 10
        assert_eq!(format(10.0 * 1024.0 * 1024.0 * 1024.0), "10Gi");
    }

    #[test]
    fn formats_with_decimal_base() {
        assert_eq!(format_with_base(1000.0, 1000.0), "1000");
        assert_eq!(format_with_base(20_000_000.0, 1000.0), "20M");
    }
}
