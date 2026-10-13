//! Queue schedules: daily start and stop times on chosen days, in local time.

use chrono::{Datelike, Days, NaiveDateTime, NaiveTime, Weekday};

/// "HH:MM" in 24-hour time.
pub fn parse_time(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
}

/// "mon,wed,fri" or "all" into the day bits used by `Schedule::days`.
pub fn parse_days(s: &str) -> Option<u8> {
    if s.trim().eq_ignore_ascii_case("all") {
        return Some(0);
    }
    s.split(',').try_fold(0u8, |bits, day| {
        let day: Weekday = day.trim().parse().ok()?;
        Some(bits | 1 << day.num_days_from_monday())
    })
}

pub fn days_text(days: u8) -> String {
    if days == 0 {
        return "every day".into();
    }
    let names = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    (0..7).filter(|i| days & (1 << i) != 0).map(|i| names[i]).collect::<Vec<_>>().join(",")
}

/// True when `time` on a selected day falls in `(from, to]`. Checking a range instead of
/// the current minute means a late tick or a sleeping computer cannot skip an event.
pub fn crossed(time: &str, days: u8, from: NaiveDateTime, to: NaiveDateTime) -> bool {
    let Some(t) = parse_time(time) else { return false };
    let mut date = from.date();
    while date <= to.date() {
        let at = date.and_time(t);
        let selected = days == 0 || days & (1 << date.weekday().num_days_from_monday()) != 0;
        if selected && from < at && at <= to {
            return true;
        }
        let Some(next) = date.checked_add_days(Days::new(1)) else { break };
        date = next;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(day: u32, h: u32, m: u32) -> NaiveDateTime {
        // 2026-10-05 is a Monday.
        NaiveDate::from_ymd_opt(2026, 10, day).unwrap().and_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn crossing_times() {
        assert!(crossed("02:00", 0, at(5, 1, 59), at(5, 2, 0)));
        assert!(!crossed("02:00", 0, at(5, 2, 0), at(5, 2, 1)), "already fired at 02:00");
        assert!(crossed("02:00", 0, at(5, 1, 0), at(5, 3, 0)), "a late tick still fires");
        assert!(crossed("23:59", 0, at(5, 23, 58), at(6, 0, 1)), "across midnight");
        assert!(crossed("00:00", 0, at(5, 23, 59), at(6, 0, 1)), "midnight itself, which is the next day's");
        let wednesday = parse_days("wed").unwrap();
        assert!(crossed("08:00", wednesday, at(5, 9, 0), at(7, 9, 0)), "asleep since Monday: Wednesday's is not skipped");
        assert!(!crossed("08:00", wednesday, at(5, 9, 0), at(7, 7, 0)), "not yet 08:00 on Wednesday");
        let monday = parse_days("mon").unwrap();
        assert!(crossed("08:00", monday, at(5, 7, 0), at(5, 9, 0)));
        assert!(!crossed("08:00", monday, at(6, 7, 0), at(6, 9, 0)), "Tuesday is not selected");
        assert!(!crossed("bad", 0, at(5, 0, 0), at(6, 0, 0)));
    }

    #[test]
    fn day_lists() {
        assert_eq!(parse_days("mon,wed,sun"), Some(0b100_0101));
        assert_eq!(parse_days("all"), Some(0));
        assert_eq!(parse_days("mon,funday"), None);
        assert_eq!(days_text(0b100_0101), "mon,wed,sun");
        assert_eq!(parse_time("7:05").map(|t| t.to_string()), Some("07:05:00".into()));
        assert_eq!(parse_time("25:00"), None);
    }
}
