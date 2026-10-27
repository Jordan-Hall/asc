//! CVSS v3 base scores, for advisories that state a vector and no label.
//!
//! Only the base metrics of CVSS 3.0 and 3.1 are read (the formulas of the specification, section 7);
//! a v2 or v4 vector gives no score, and the advisory then has no severity unless it states a label.

use asc_core::policy::DepSeverity;

/// The score of a CVSS v3 vector (`CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H`), or `None` when
/// it is not one or misses a base metric.
pub fn score(vector: &str) -> Option<f64> {
    let mut parts = vector.trim().split('/');
    let head = parts.next()?;
    if !matches!(head, "CVSS:3.0" | "CVSS:3.1") {
        return None;
    }
    let (mut av, mut ac, mut pr, mut ui, mut scope, mut c, mut i, mut a) =
        (None, None, None, None, None, None, None, None);
    for m in parts {
        let (k, v) = m.split_once(':')?;
        match k {
            "AV" => av = Some(v),
            "AC" => ac = Some(v),
            "PR" => pr = Some(v),
            "UI" => ui = Some(v),
            "S" => scope = Some(v),
            "C" => c = Some(v),
            "I" => i = Some(v),
            "A" => a = Some(v),
            _ => {}
        }
    }
    let changed = match scope? {
        "U" => false,
        "C" => true,
        _ => return None,
    };
    let av = match av? {
        "N" => 0.85,
        "A" => 0.62,
        "L" => 0.55,
        "P" => 0.2,
        _ => return None,
    };
    let ac = match ac? {
        "L" => 0.77,
        "H" => 0.44,
        _ => return None,
    };
    let pr = match (pr?, changed) {
        ("N", _) => 0.85,
        ("L", false) => 0.62,
        ("L", true) => 0.68,
        ("H", false) => 0.27,
        ("H", true) => 0.5,
        _ => return None,
    };
    let ui = match ui? {
        "N" => 0.85,
        "R" => 0.62,
        _ => return None,
    };
    let cia = |v: Option<&str>| -> Option<f64> {
        match v? {
            "H" => Some(0.56),
            "L" => Some(0.22),
            "N" => Some(0.0),
            _ => None,
        }
    };
    let (c, i, a) = (cia(c)?, cia(i)?, cia(a)?);
    let iss = 1.0 - (1.0 - c) * (1.0 - i) * (1.0 - a);
    let impact = if changed {
        7.52 * (iss - 0.029) - 3.25 * (iss - 0.02).powi(15)
    } else {
        6.42 * iss
    };
    if impact <= 0.0 {
        return Some(0.0);
    }
    let exploitability = 8.22 * av * ac * pr * ui;
    let raw: f64 = if changed {
        1.08 * (impact + exploitability)
    } else {
        impact + exploitability
    };
    Some(round_up(raw.min(10.0)))
}

/// The smallest number with one decimal that is not below `x` (the specification's `Roundup`).
fn round_up(x: f64) -> f64 {
    let int = (x * 100_000.0).round() as i64;
    if int % 10_000 == 0 {
        int as f64 / 100_000.0
    } else {
        ((int / 10_000) + 1) as f64 / 10.0
    }
}

/// The qualitative rating of a base score: 0.1-3.9 low, 4.0-6.9 moderate, 7.0-8.9 high, 9.0-10 critical.
pub fn band(score: f64) -> Option<DepSeverity> {
    match score {
        s if s >= 9.0 => Some(DepSeverity::Critical),
        s if s >= 7.0 => Some(DepSeverity::High),
        s if s >= 4.0 => Some(DepSeverity::Moderate),
        s if s > 0.0 => Some(DepSeverity::Low),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_match_the_published_ones() {
        for (v, want) in [
            ("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H", 9.8),
            ("CVSS:3.1/AV:N/AC:L/PR:N/UI:R/S:C/C:L/I:L/A:N", 6.1),
            ("CVSS:3.1/AV:L/AC:L/PR:L/UI:N/S:U/C:H/I:H/A:H", 7.8),
            ("CVSS:3.0/AV:N/AC:L/PR:L/UI:N/S:U/C:H/I:N/A:N", 6.5),
            ("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:C/C:H/I:H/A:H", 10.0),
            ("CVSS:3.1/AV:P/AC:H/PR:H/UI:R/S:U/C:L/I:N/A:N", 1.6),
            ("CVSS:3.1/AV:N/AC:L/PR:L/UI:N/S:C/C:L/I:L/A:N", 6.4),
        ] {
            assert_eq!(score(v), Some(want), "{v}");
        }
        assert_eq!(
            score("CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:N/I:N/A:N"),
            Some(0.0)
        );
    }

    #[test]
    fn what_is_not_a_complete_v3_vector_has_no_score() {
        for v in [
            "",
            "AV:N/AC:L/Au:N/C:P/I:P/A:P",
            "CVSS:4.0/AV:N/AC:L/AT:N/PR:N/UI:N/VC:H/VI:H/VA:H/SC:N/SI:N/SA:N",
            "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H",
            "CVSS:3.1/AV:X/AC:L/PR:N/UI:N/S:U/C:H/I:H/A:H",
            "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:Q/C:H/I:H/A:H",
            "CVSS:3.1/AV:N/AC:L/PR:N/UI:N/S:U/C:H/I:H/A",
        ] {
            assert_eq!(score(v), None, "{v:?}");
        }
    }

    #[test]
    fn the_bands_follow_the_specification() {
        assert_eq!(band(0.0), None);
        assert_eq!(band(0.1), Some(DepSeverity::Low));
        assert_eq!(band(3.9), Some(DepSeverity::Low));
        assert_eq!(band(4.0), Some(DepSeverity::Moderate));
        assert_eq!(band(6.9), Some(DepSeverity::Moderate));
        assert_eq!(band(7.0), Some(DepSeverity::High));
        assert_eq!(band(8.9), Some(DepSeverity::High));
        assert_eq!(band(9.0), Some(DepSeverity::Critical));
        assert_eq!(band(10.0), Some(DepSeverity::Critical));
    }
}
