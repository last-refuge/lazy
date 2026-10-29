//! Time labels for the timeline, in the person's local time zone.
//!
//! Timestamps from the server are for display only; they never decide message order.

use jiff::civil::Date;
use jiff::tz::TimeZone;
use jiff::{Timestamp as Instant, Zoned};

/// Converts a protocol timestamp into a zoned time. Out-of-range values clamp to the epoch
/// rather than failing, since a label is not worth an error path.
pub fn to_zoned(timestamp: lazy_domain::Timestamp, zone: &TimeZone) -> Zoned {
    let nanos = timestamp.as_offset_date_time().unix_timestamp_nanos();
    Instant::from_nanosecond(nanos)
        .unwrap_or(Instant::UNIX_EPOCH)
        .to_zoned(zone.clone())
}

/// Clock time such as `3:42 PM`.
pub fn clock_label(time: &Zoned) -> String {
    time.strftime("%-I:%M %p").to_string()
}

/// Heading for a run of messages sent on `day`: `Today`, `Yesterday`, a weekday and date, or
/// that plus the year when it is not the current year.
pub fn day_label(day: Date, today: Date) -> String {
    if day == today {
        return "Today".to_owned();
    }
    if today.yesterday().is_ok_and(|yesterday| yesterday == day) {
        return "Yesterday".to_owned();
    }
    if day.year() == today.year() {
        day.strftime("%A, %B %-d").to_string()
    } else {
        day.strftime("%A, %B %-d, %Y").to_string()
    }
}

/// How long ago `then` was, for the line under a thread's root: `just now`, `5 minutes ago`,
/// `3 hours ago`, `yesterday`, a weekday for the last week, else the date (with the year when it
/// is not the current one). A time in the future (clock skew) reads as `just now`.
pub fn ago_label(then: &Zoned, now: &Zoned) -> String {
    let seconds = now.timestamp().duration_since(then.timestamp()).as_secs();
    let plural = |n: i64, unit: &str| {
        if n == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{n} {unit}s ago")
        }
    };
    if seconds < 60 {
        return "just now".to_owned();
    }
    if seconds < 3_600 {
        return plural(seconds / 60, "minute");
    }
    let today = now.date();
    let day = then.date();
    if day == today {
        return plural(seconds / 3_600, "hour");
    }
    if today.yesterday().is_ok_and(|yesterday| yesterday == day) {
        return "yesterday".to_owned();
    }
    let days = (today - day).get_days();
    if (2..7).contains(&days) {
        return then.strftime("%A").to_string();
    }
    if day.year() == today.year() {
        then.strftime("%b %-d").to_string()
    } else {
        then.strftime("%b %-d, %Y").to_string()
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;

    #[test]
    fn day_labels_are_relative_then_absolute() {
        let today = date(2026, 10, 28);
        assert_eq!(day_label(date(2026, 10, 28), today), "Today");
        assert_eq!(day_label(date(2026, 10, 27), today), "Yesterday");
        assert_eq!(day_label(date(2026, 10, 20), today), "Tuesday, October 20");
        assert_eq!(
            day_label(date(2025, 12, 31), today),
            "Wednesday, December 31, 2025"
        );
    }

    #[test]
    fn clock_labels_use_twelve_hour_time() {
        let zone = TimeZone::UTC;
        let time = Instant::from_second(1_800_000_000).unwrap().to_zoned(zone);
        assert_eq!(clock_label(&time), "8:00 AM");
    }

    #[test]
    fn protocol_timestamps_convert_in_the_requested_zone() {
        let stamp = lazy_domain::Timestamp::from(
            time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
        );
        let zoned = to_zoned(stamp, &TimeZone::get("America/Phoenix").unwrap());
        assert_eq!(clock_label(&zoned), "1:00 AM");
    }

    fn at(seconds_ago: i64, now: &Zoned) -> Zoned {
        now.checked_sub(jiff::SignedDuration::from_secs(seconds_ago))
            .unwrap()
    }

    #[test]
    fn recent_times_read_as_minutes_then_hours() {
        let now = date(2026, 10, 28)
            .at(15, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        assert_eq!(ago_label(&at(5, &now), &now), "just now");
        assert_eq!(
            ago_label(&at(-30, &now), &now),
            "just now",
            "the future is now"
        );
        assert_eq!(ago_label(&at(60, &now), &now), "1 minute ago");
        assert_eq!(ago_label(&at(5 * 60 + 20, &now), &now), "5 minutes ago");
        assert_eq!(ago_label(&at(3_600, &now), &now), "1 hour ago");
        assert_eq!(ago_label(&at(3 * 3_600, &now), &now), "3 hours ago");
    }

    #[test]
    fn older_times_read_as_yesterday_a_weekday_or_a_date() {
        let now = date(2026, 10, 28)
            .at(15, 0, 0, 0)
            .to_zoned(TimeZone::UTC)
            .unwrap();
        // 20 hours ago crosses midnight: yesterday, not "20 hours ago".
        assert_eq!(ago_label(&at(20 * 3_600, &now), &now), "yesterday");
        assert_eq!(ago_label(&at(3 * 86_400, &now), &now), "Sunday");
        assert_eq!(ago_label(&at(10 * 86_400, &now), &now), "Oct 18");
        assert_eq!(ago_label(&at(400 * 86_400, &now), &now), "Sep 23, 2025");
    }
}
