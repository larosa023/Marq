//! Reading and writing real-world lengths for the Measure tool.
//! Imperial lengths are kept in inches, metric in millimetres.

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Units {
    Imperial, // value in inches
    Metric,   // value in millimetres
}

/// A drawing scale: how many real units one image pixel is.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Scale {
    pub units: Units,
    pub per_px: f64,
}

/// Parse what someone types for a length: 20' 6", 20'-6 1/2", 20.5', 246", 20 ft 6 in,
/// 6", 20 (bare numbers are feet), 5 m, 5.25m, 500 mm, 45 cm.
pub fn parse(input: &str) -> Option<(f64, Units)> {
    let s = input.trim().to_lowercase().replace(['\u{2019}', '\u{2032}'], "'").replace(['\u{201D}', '\u{2033}'], "\"");
    if s.is_empty() {
        return None;
    }
    // Metric first.
    for (suffix, mm) in [("mm", 1.0), ("cm", 10.0), ("m", 1000.0)] {
        if let Some(num) = s.strip_suffix(suffix) {
            let v: f64 = num.trim().parse().ok()?;
            return (v > 0.0).then_some((v * mm, Units::Metric));
        }
    }
    // Imperial: normalize words to ' and ".
    let s = s
        .replace("feet", "'")
        .replace("foot", "'")
        .replace("ft", "'")
        .replace("inches", "\"")
        .replace("inch", "\"")
        .replace("in", "\"")
        .replace("''", "\"");
    let (feet_part, inch_part) = match s.find('\'') {
        Some(i) => (&s[..i], &s[i + 1..]),
        None if s.contains('"') => ("", s.as_str()),
        None => (s.as_str(), ""), // bare number: feet
    };
    let feet = if feet_part.trim().is_empty() { 0.0 } else { number(feet_part)? };
    let inch_str = inch_part.trim().trim_start_matches('-').trim().trim_end_matches('"').trim();
    let inches = if inch_str.is_empty() { 0.0 } else { number(inch_str)? };
    let total = feet * 12.0 + inches;
    (total > 0.0).then_some((total, Units::Imperial))
}

/// "6", "6.5", "1/2", "6 1/2" or "6-1/2".
fn number(s: &str) -> Option<f64> {
    let s = s.trim();
    let parts: Vec<&str> = s.split(|c: char| c == ' ' || c == '-').filter(|p| !p.is_empty()).collect();
    let mut total = 0.0;
    for p in parts {
        if let Some((a, b)) = p.split_once('/') {
            let (a, b): (f64, f64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
            if b == 0.0 {
                return None;
            }
            total += a / b;
        } else {
            total += p.parse::<f64>().ok()?;
        }
    }
    Some(total)
}

/// Format a length the way it's written on drawings: 12'-6 1/4", 8 1/2", 3.25 m, 450 mm.
pub fn format(value: f64, units: Units) -> String {
    match units {
        Units::Imperial => {
            let q = (value * 4.0).round() as i64; // quarter inches
            let feet = q / 48;
            let rem = q % 48;
            let whole = rem / 4;
            let frac = match rem % 4 {
                1 => " 1/4",
                2 => " 1/2",
                3 => " 3/4",
                _ => "",
            };
            if feet > 0 {
                format!("{feet}'-{whole}{frac}\"")
            } else {
                format!("{whole}{frac}\"")
            }
        }
        Units::Metric => {
            if value >= 1000.0 {
                format!("{:.2} m", value / 1000.0)
            } else {
                format!("{} mm", value.round() as i64)
            }
        }
    }
}

/// Label for a line `px` image pixels long.
pub fn label(px: f64, scale: Option<Scale>) -> String {
    match scale {
        Some(s) => format(px * s.per_px, s.units),
        None => format!("{} px", px.round() as i64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn inches(s: &str) -> f64 {
        match parse(s) {
            Some((v, Units::Imperial)) => v,
            other => panic!("{s:?} -> {other:?}"),
        }
    }
    #[test]
    fn imperial() {
        assert_eq!(inches("20'"), 240.0);
        assert_eq!(inches("20' 6\""), 246.0);
        assert_eq!(inches("20'-6\""), 246.0);
        assert_eq!(inches("20'6"), 246.0);
        assert_eq!(inches("12'-6 1/2\""), 150.5);
        assert_eq!(inches("6\""), 6.0);
        assert_eq!(inches("6 1/2\""), 6.5);
        assert_eq!(inches("20"), 240.0);
        assert_eq!(inches("20.5'"), 246.0);
        assert_eq!(inches("20 ft 6 in"), 246.0);
        assert_eq!(inches("3 feet"), 36.0);
        assert_eq!(inches("20\u{2019} 6\u{201D}"), 246.0);
    }
    #[test]
    fn metric() {
        assert_eq!(parse("5 m"), Some((5000.0, Units::Metric)));
        assert_eq!(parse("5.25m"), Some((5250.0, Units::Metric)));
        assert_eq!(parse("450 mm"), Some((450.0, Units::Metric)));
        assert_eq!(parse("45cm"), Some((450.0, Units::Metric)));
    }
    #[test]
    fn junk() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("abc"), None);
        assert_eq!(parse("0"), None);
        assert_eq!(parse("1/0\""), None);
    }
    #[test]
    fn formatting() {
        assert_eq!(format(246.0, Units::Imperial), "20'-6\"");
        assert_eq!(format(150.26, Units::Imperial), "12'-6 1/4\"");
        assert_eq!(format(8.5, Units::Imperial), "8 1/2\"");
        assert_eq!(format(240.0, Units::Imperial), "20'-0\"");
        assert_eq!(format(3250.0, Units::Metric), "3.25 m");
        assert_eq!(format(450.4, Units::Metric), "450 mm");
        assert_eq!(label(100.0, None), "100 px");
        let s = Scale { units: Units::Imperial, per_px: 2.46 };
        assert_eq!(label(100.0, Some(s)), "20'-6\"");
    }
}
