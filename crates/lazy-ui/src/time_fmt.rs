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
}
