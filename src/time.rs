//! Timestamps: parse transcript times, format them in the user's local time zone.

use chrono::{DateTime, Datelike, Local, TimeZone};

/// Unix seconds from an RFC 3339 timestamp such as `2026-03-28T10:15:30.123Z`.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp())
}

fn local(ts: i64) -> Option<DateTime<Local>> {
    Local.timestamp_opt(ts, 0).single()
}

/// `Tue Sep 30 14:05`, with the year added when it isn't the current one.
pub fn datetime(ts: i64, now: i64) -> String {
    let (Some(t), Some(n)) = (local(ts), local(now)) else {
        return "?".into();
    };
    if t.year() == n.year() {
        t.format("%a %b %d %H:%M").to_string()
    } else {
        t.format("%a %b %d %Y").to_string()
    }
}

/// When a session was active: `Wed Sep 30 14:05 → 16:40` within a day, otherwise both ends.
pub fn span(start: i64, end: i64, now: i64) -> String {
    let (from, to) = (datetime(start, now), datetime(end, now));
    match (local(start), local(end)) {
        // Also a day in an earlier year, which `datetime` shows without the time.
        _ if from == to => from,
        (Some(s), Some(e)) if s.date_naive() == e.date_naive() => {
            format!("{from} → {}", e.format("%H:%M"))
        }
        _ => format!("{from} → {to}"),
    }
}

/// Short relative day for list views: `today`, `yesterday`, `3d ago`, `2w ago`, `Mar 28`, `Mar 2025`.
pub fn relative(ts: i64, now: i64) -> String {
    let (Some(t), Some(n)) = (local(ts), local(now)) else {
        return "?".into();
    };
    let days = (n.date_naive() - t.date_naive()).num_days();
    match days {
        i64::MIN..=0 => "today".into(),
        1 => "yesterday".into(),
        2..=6 => format!("{days}d ago"),
        7..=29 => format!("{}w ago", days / 7),
        _ if t.year() == n.year() => t.format("%b %d").to_string(),
        _ => t.format("%b %Y").to_string(),
    }
}

/// Seconds since the epoch, now.
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_claude_code_timestamps() {
        assert_eq!(
            parse_rfc3339("2026-03-28T10:15:30.123Z"),
            Some(1_774_692_930)
        );
        assert_eq!(
            parse_rfc3339("2026-03-28T12:15:30+02:00"),
            Some(1_774_692_930)
        );
        assert_eq!(parse_rfc3339("not a date"), None);
        assert_eq!(parse_rfc3339(""), None);
    }

    #[test]
    fn relative_days_use_local_calendar_days() {
        let now = Local
            .with_ymd_and_hms(2026, 10, 1, 9, 0, 0)
            .unwrap()
            .timestamp();
        let at = |d: u32, h: u32| {
            Local
                .with_ymd_and_hms(2026, 10, d, h, 0, 0)
                .unwrap()
                .timestamp()
        };
        assert_eq!(relative(at(1, 0), now), "today");
        assert_eq!(relative(now + 3600, now), "today");
        let sept = |d: u32| {
            Local
                .with_ymd_and_hms(2026, 9, d, 23, 59, 0)
                .unwrap()
                .timestamp()
        };
        assert_eq!(relative(sept(30), now), "yesterday");
        assert_eq!(relative(sept(27), now), "4d ago");
        assert_eq!(relative(sept(10), now), "3w ago");
        let old = Local
            .with_ymd_and_hms(2026, 3, 28, 12, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(relative(old, now), "Mar 28");
        let older = Local
            .with_ymd_and_hms(2025, 3, 28, 12, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(relative(older, now), "Mar 2025");
    }

    #[test]
    fn datetime_adds_year_only_when_needed() {
        let now = Local
            .with_ymd_and_hms(2026, 10, 1, 9, 0, 0)
            .unwrap()
            .timestamp();
        let t = Local
            .with_ymd_and_hms(2026, 9, 30, 14, 5, 0)
            .unwrap()
            .timestamp();
        assert_eq!(datetime(t, now), "Wed Sep 30 14:05");
        let t = Local
            .with_ymd_and_hms(2025, 9, 30, 14, 5, 0)
            .unwrap()
            .timestamp();
        assert_eq!(datetime(t, now), "Tue Sep 30 2025");
    }

    #[test]
    fn span_leaves_out_what_repeats() {
        let now = Local
            .with_ymd_and_hms(2026, 10, 1, 9, 0, 0)
            .unwrap()
            .timestamp();
        let at = |y: i32, m: u32, d: u32, h: u32, min: u32| {
            Local
                .with_ymd_and_hms(y, m, d, h, min, 0)
                .unwrap()
                .timestamp()
        };
        let a = at(2026, 9, 30, 14, 5);
        assert_eq!(
            span(a, at(2026, 9, 30, 16, 40), now),
            "Wed Sep 30 14:05 → 16:40"
        );
        assert_eq!(span(a, a + 20, now), "Wed Sep 30 14:05");
        assert_eq!(
            span(a, at(2026, 10, 1, 8, 0), now),
            "Wed Sep 30 14:05 → Thu Oct 01 08:00"
        );
        let old = at(2025, 3, 28, 10, 0);
        assert_eq!(span(old, old + 3600, now), "Fri Mar 28 2025");
        assert_eq!(
            span(old, at(2025, 4, 2, 10, 0), now),
            "Fri Mar 28 2025 → Wed Apr 02 2025"
        );
    }
}
