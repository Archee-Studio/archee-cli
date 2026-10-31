//! Lease expiry only needs whole seconds; this avoids a date crate for one comparison.

/// Seconds since the Unix epoch for `YYYY-MM-DDTHH:MM:SS[.f](Z|±HH:MM)`.
pub(crate) fn epoch_seconds(text: &str) -> Option<i64> {
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    let zone = text.get(19..)?;
    let zone = zone.find(['Z', '+', '-']).and_then(|at| zone.get(at..))?;
    let offset = if zone == "Z" {
        0
    } else {
        let sign = if zone.starts_with('-') { -1 } else { 1 };
        let hours = zone.get(1..3)?.parse::<i64>().ok()?;
        let minutes = zone.get(4..6)?.parse::<i64>().ok()?;
        sign * (hours * 3600 + minutes * 60)
    };
    // Days from civil, Howard Hinnant's algorithm.
    let shifted = if month <= 2 { year - 1 } else { year };
    let era = shifted.div_euclid(400);
    let year_of_era = shifted - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

#[cfg(test)]
mod tests {
    use super::epoch_seconds;

    #[test]
    fn parses_what_the_backend_sends() {
        assert_eq!(epoch_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_seconds("2026-10-31T12:00:00Z"), Some(1_793_448_000));
        assert_eq!(
            epoch_seconds("2026-10-31T12:00:00.123456+00:00"),
            Some(1_793_448_000)
        );
        assert_eq!(
            epoch_seconds("2026-10-31T19:00:00+07:00"),
            Some(1_793_448_000)
        );
        assert_eq!(epoch_seconds("soon"), None);
    }
}
