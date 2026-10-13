//! Human-readable sizes, speeds, times and rates.

pub fn human(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = bytes;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{v:.0} B") } else { format!("{v:.1} {}", UNITS[unit]) }
}

pub fn clock(secs: f64) -> String {
    let s = secs.round() as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// "2M", "500K", "1.5M", "1048576" (bytes per second, binary units); "off" or "0" is 0.
pub fn parse_rate(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("off") {
        return Ok(0);
    }
    let (number, scale) = match s.char_indices().last() {
        Some((i, 'k' | 'K')) => (&s[..i], 1024.0),
        Some((i, 'm' | 'M')) => (&s[..i], 1024.0 * 1024.0),
        Some((i, 'g' | 'G')) => (&s[..i], 1024.0 * 1024.0 * 1024.0),
        _ => (s, 1.0),
    };
    let value: f64 = number.parse().map_err(|_| format!("not a rate: {s} (try 500K, 2M or off)"))?;
    if !(value >= 0.0 && value.is_finite()) {
        return Err(format!("not a rate: {s}"));
    }
    Ok((value * scale) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates() {
        assert_eq!(parse_rate("2M"), Ok(2 << 20));
        assert_eq!(parse_rate("500k"), Ok(500 << 10));
        assert_eq!(parse_rate("1.5M"), Ok(3 << 19));
        assert_eq!(parse_rate("4096"), Ok(4096));
        assert_eq!(parse_rate("off"), Ok(0));
        assert!(parse_rate("fast").is_err());
        assert!(parse_rate("-1M").is_err());
        // Either case, spaces around it, and each unit.
        assert_eq!(parse_rate("500K"), Ok(500 << 10));
        assert_eq!(parse_rate("2m"), Ok(2 << 20));
        assert_eq!((parse_rate("1g"), parse_rate("1G")), (Ok(1 << 30), Ok(1 << 30)));
        assert_eq!((parse_rate("OFF"), parse_rate("0"), parse_rate(" 2M ")), (Ok(0), Ok(0), Ok(2 << 20)));
        // What parses as a number but is no rate.
        for not_a_rate in ["inf", "infM", "nan", "", "M"] {
            assert!(parse_rate(not_a_rate).is_err(), "{not_a_rate:?}");
        }
    }

    #[test]
    fn formats() {
        assert_eq!(human(512.0), "512 B");
        assert_eq!(human(1536.0), "1.5 KiB");
        assert_eq!(clock(61.0), "1:01");
        assert_eq!(clock(3661.0), "1:01:01");
        // Where one unit ends and the next begins.
        assert_eq!((human(1023.0), human(1024.0)), ("1023 B".into(), "1.0 KiB".into()));
        assert_eq!((human(1048576.0), human(1.5 * 1073741824.0)), ("1.0 MiB".into(), "1.5 GiB".into()));
        assert_eq!((human(1099511627776.0), human(2048.0 * 1099511627776.0)), ("1.0 TiB".into(), "2048.0 TiB".into()));
        assert_eq!((clock(0.0), clock(59.6), clock(3599.0), clock(3600.0)), ("0:00".into(), "1:00".into(), "59:59".into(), "1:00:00".into()));
    }
}
