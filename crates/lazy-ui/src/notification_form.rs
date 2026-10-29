//! Pure helpers for the preferences dialog: the choices of how long to pause or keep a status,
//! turned into moments in the person's own time, and the quiet hours' times of day as text.
//!
//! Everything takes the current time and the person's time zone as arguments, so each rule is a
//! unit test; the dialog passes the real clock.

use jiff::tz::TimeZone;
use jiff::{Span, Timestamp as Instant, Zoned};
use lazy_domain::Timestamp;
use lazy_domain::notification::QuietHours;

use crate::states;

/// How long to pause notifications for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseFor {
    HalfAnHour,
    OneHour,
    TwoHours,
    /// Until 9:00 the next morning, in the person's time.
    TomorrowMorning,
    /// Until the person turns it back on: a long snooze, since the setting is a moment, not a
    /// switch (see [`UNTIL_RESUMED`]).
    UntilResumed,
}

impl PauseFor {
    pub const ALL: [PauseFor; 5] = [
        PauseFor::HalfAnHour,
        PauseFor::OneHour,
        PauseFor::TwoHours,
        PauseFor::TomorrowMorning,
        PauseFor::UntilResumed,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::HalfAnHour => "30 minutes",
            Self::OneHour => "1 hour",
            Self::TwoHours => "2 hours",
            Self::TomorrowMorning => "Until tomorrow morning",
            Self::UntilResumed => "Until I turn it back on",
        }
    }
}

/// "Until I turn it back on" is a snooze this many years long; Resume ends it.
pub const UNTIL_RESUMED_YEARS: i64 = 5;

/// How long after now "until I turn it back on" lasts, for tests and the label.
pub const UNTIL_RESUMED: &str = "until you turn it back on";

/// How long to keep a status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClearAfter {
    DontClear,
    HalfAnHour,
    OneHour,
    FourHours,
    /// Until the end of today, in the person's time.
    Today,
    /// Until the end of this week (Sunday night), in the person's time.
    ThisWeek,
}

impl ClearAfter {
    pub const ALL: [ClearAfter; 6] = [
        ClearAfter::DontClear,
        ClearAfter::HalfAnHour,
        ClearAfter::OneHour,
        ClearAfter::FourHours,
        ClearAfter::Today,
        ClearAfter::ThisWeek,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::DontClear => "Don't clear",
            Self::HalfAnHour => "30 minutes",
            Self::OneHour => "1 hour",
            Self::FourHours => "4 hours",
            Self::Today => "Today",
            Self::ThisWeek => "This week",
        }
    }
}

fn zoned(now: Timestamp, zone: &TimeZone) -> Zoned {
    Instant::from_nanosecond(now.as_offset_date_time().unix_timestamp_nanos())
        .unwrap_or(Instant::UNIX_EPOCH)
        .to_zoned(zone.clone())
}

fn stamp(moment: &Zoned) -> Timestamp {
    let nanos = moment.timestamp().as_nanosecond();
    Timestamp::from(
        time::OffsetDateTime::from_unix_timestamp_nanos(nanos)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH),
    )
}

/// When a pause of `choice` started at `now` ends.
pub fn pause_until(choice: PauseFor, now: Timestamp, zone: &TimeZone) -> Timestamp {
    let start = zoned(now, zone);
    let end = match choice {
        PauseFor::HalfAnHour => start.checked_add(Span::new().minutes(30)),
        PauseFor::OneHour => start.checked_add(Span::new().hours(1)),
        PauseFor::TwoHours => start.checked_add(Span::new().hours(2)),
        PauseFor::TomorrowMorning => start
            .date()
            .tomorrow()
            .ok()
            .and_then(|day| day.at(9, 0, 0, 0).to_zoned(zone.clone()).ok())
            .ok_or(jiff::Error::from_args(format_args!("no tomorrow"))),
        PauseFor::UntilResumed => start.checked_add(Span::new().days(365 * UNTIL_RESUMED_YEARS)),
    };
    // A rule that cannot be worked out (the end of time zones' tables) is a plain hour.
    stamp(&end.unwrap_or_else(|_| {
        start
            .checked_add(Span::new().hours(1))
            .unwrap_or_else(|_| start.clone())
    }))
}

/// When a status kept for `choice` from `now` ends, or `None` to keep it until changed.
pub fn status_until(choice: ClearAfter, now: Timestamp, zone: &TimeZone) -> Option<Timestamp> {
    let start = zoned(now, zone);
    let end = match choice {
        ClearAfter::DontClear => return None,
        ClearAfter::HalfAnHour => start.checked_add(Span::new().minutes(30)).ok(),
        ClearAfter::OneHour => start.checked_add(Span::new().hours(1)).ok(),
        ClearAfter::FourHours => start.checked_add(Span::new().hours(4)).ok(),
        ClearAfter::Today => start
            .date()
            .tomorrow()
            .ok()
            .and_then(|day| day.at(0, 0, 0, 0).to_zoned(zone.clone()).ok()),
        ClearAfter::ThisWeek => {
            let days_to_sunday = 7 - i64::from(start.date().weekday().to_monday_one_offset());
            start
                .date()
                .checked_add(Span::new().days(days_to_sunday + 1))
                .ok()
                .and_then(|day| day.at(0, 0, 0, 0).to_zoned(zone.clone()).ok())
        }
    };
    end.map(|moment| stamp(&moment))
}

/// A moment for "paused until ...": the time of day when it is today, `tomorrow 9:00 AM`, or a
/// weekday and time within the week, else a date.
pub fn moment_label(until: Timestamp, now: Timestamp, zone: &TimeZone) -> String {
    let (moment, today) = (zoned(until, zone), zoned(now, zone));
    let clock = moment.strftime("%-I:%M %p").to_string();
    let days = (moment.date() - today.date()).get_days();
    match days {
        ..=0 => clock,
        1 => format!("tomorrow {clock}"),
        2..=6 => format!("{} {clock}", moment.strftime("%A")),
        _ => format!("{} {clock}", moment.strftime("%b %-d")),
    }
}

/// `HH:MM` for minutes after midnight.
pub fn clock_text(minutes: u16) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

/// Minutes after midnight for `H:MM`, `HH:MM`, or `HHMM`-free text like `7:30`; `None` when it is
/// not a time of day.
pub fn parse_clock(text: &str) -> Option<u16> {
    let (hours, minutes) = text.trim().split_once(':')?;
    let (hours, minutes): (u16, u16) = (hours.trim().parse().ok()?, minutes.trim().parse().ok()?);
    (hours < 24 && minutes < 60 && text.trim().len() <= 5).then_some(hours * 60 + minutes)
}

/// The person's offset from UTC at `now`, in minutes, as quiet hours record it.
pub fn utc_offset_minutes(now: Timestamp, zone: &TimeZone) -> i16 {
    let moment = zoned(now, zone);
    (moment.offset().seconds() / 60) as i16
}

/// `UTC+5:30`, `UTC−7`, `UTC`.
pub fn offset_text(minutes: i16) -> String {
    if minutes == 0 {
        return "UTC".to_owned();
    }
    let sign = if minutes < 0 { '−' } else { '+' };
    let (hours, rest) = ((minutes.abs() / 60), (minutes.abs() % 60));
    if rest == 0 {
        format!("UTC{sign}{hours}")
    } else {
        format!("UTC{sign}{hours}:{rest:02}")
    }
}

/// The days of the week, Monday first, as the quiet hours' bit mask numbers them.
pub const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/// A sentence about the quiet hours, for the dialog: `22:00 to 07:00, every day`.
pub fn quiet_summary(hours: &QuietHours) -> String {
    let days = if hours.days == QuietHours::EVERY_DAY {
        "every day".to_owned()
    } else {
        DAYS.iter()
            .enumerate()
            .filter(|(bit, _)| hours.days & (1 << bit) != 0)
            .map(|(_, name)| *name)
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "{} to {}, {days}",
        clock_text(hours.start_minute),
        clock_text(hours.end_minute)
    )
}

/// What the pause section says: a snooze that is running (its end in the person's time, or
/// "until you turn it back on" for a pause of years), quiet hours that hold notifications back
/// now, or that they are on.
pub fn pause_line(
    until: Option<Timestamp>,
    paused_now: bool,
    now: Timestamp,
    zone: &TimeZone,
) -> String {
    match until {
        Some(until)
            if until.as_offset_date_time()
                > now.as_offset_date_time() + time::Duration::days(365) =>
        {
            states::paused_until_resumed()
        }
        Some(until) => states::paused_until(&moment_label(until, now, zone)),
        None if paused_now => states::PAUSED_BY_QUIET_HOURS.to_owned(),
        None => states::NOT_PAUSED.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;

    fn at(year: i16, month: i8, day: i8, hour: i8, minute: i8, zone: &TimeZone) -> Timestamp {
        stamp(
            &date(year, month, day)
                .at(hour, minute, 0, 0)
                .to_zoned(zone.clone())
                .unwrap(),
        )
    }

    fn local(moment: Timestamp, zone: &TimeZone) -> String {
        zoned(moment, zone).strftime("%Y-%m-%d %H:%M").to_string()
    }

    #[test]
    fn short_pauses_are_minutes_and_hours_from_now() {
        let zone = TimeZone::get("America/Phoenix").unwrap();
        let now = at(2026, 10, 28, 15, 0, &zone);
        assert_eq!(
            local(pause_until(PauseFor::HalfAnHour, now, &zone), &zone),
            "2026-10-28 15:30"
        );
        assert_eq!(
            local(pause_until(PauseFor::OneHour, now, &zone), &zone),
            "2026-10-28 16:00"
        );
        assert_eq!(
            local(pause_until(PauseFor::TwoHours, now, &zone), &zone),
            "2026-10-28 17:00"
        );
    }

    #[test]
    fn until_tomorrow_morning_is_nine_local_the_next_day() {
        let zone = TimeZone::get("America/Toronto").unwrap();
        let late = at(2026, 10, 28, 23, 45, &zone);
        assert_eq!(
            local(pause_until(PauseFor::TomorrowMorning, late, &zone), &zone),
            "2026-10-29 09:00"
        );
        let early = at(2026, 10, 28, 1, 5, &zone);
        assert_eq!(
            local(pause_until(PauseFor::TomorrowMorning, early, &zone), &zone),
            "2026-10-29 09:00",
            "tomorrow, even when it is still the small hours of today"
        );
        // Across the clocks going back (Toronto, 2026-11-01), 9:00 is still 9:00.
        let before_change = at(2026, 10, 31, 20, 0, &zone);
        assert_eq!(
            local(
                pause_until(PauseFor::TomorrowMorning, before_change, &zone),
                &zone
            ),
            "2026-11-01 09:00"
        );
    }

    #[test]
    fn until_resumed_is_far_away_and_a_pause_always_ends_after_it_starts() {
        let zone = TimeZone::UTC;
        let now = at(2026, 10, 28, 15, 0, &zone);
        let far = pause_until(PauseFor::UntilResumed, now, &zone);
        assert!(
            far.as_offset_date_time() > now.as_offset_date_time() + time::Duration::days(365 * 4)
        );
        for choice in PauseFor::ALL {
            assert!(pause_until(choice, now, &zone) > now, "{choice:?}");
        }
    }

    #[test]
    fn a_status_is_kept_until_the_chosen_moment() {
        let zone = TimeZone::get("America/Phoenix").unwrap();
        // Wednesday.
        let now = at(2026, 10, 28, 15, 10, &zone);
        assert_eq!(status_until(ClearAfter::DontClear, now, &zone), None);
        let local_of = |choice| local(status_until(choice, now, &zone).unwrap(), &zone);
        assert_eq!(local_of(ClearAfter::HalfAnHour), "2026-10-28 15:40");
        assert_eq!(local_of(ClearAfter::OneHour), "2026-10-28 16:10");
        assert_eq!(local_of(ClearAfter::FourHours), "2026-10-28 19:10");
        assert_eq!(
            local_of(ClearAfter::Today),
            "2026-10-29 00:00",
            "the end of today"
        );
        assert_eq!(
            local_of(ClearAfter::ThisWeek),
            "2026-11-02 00:00",
            "the end of Sunday"
        );
    }

    #[test]
    fn this_week_on_a_sunday_ends_that_night_and_on_a_monday_a_week_later() {
        let zone = TimeZone::UTC;
        let sunday = at(2026, 11, 1, 10, 0, &zone);
        assert_eq!(
            local(
                status_until(ClearAfter::ThisWeek, sunday, &zone).unwrap(),
                &zone
            ),
            "2026-11-02 00:00"
        );
        let monday = at(2026, 10, 26, 10, 0, &zone);
        assert_eq!(
            local(
                status_until(ClearAfter::ThisWeek, monday, &zone).unwrap(),
                &zone
            ),
            "2026-11-02 00:00"
        );
    }

    #[test]
    fn times_of_day_read_and_write_as_hours_and_minutes() {
        assert_eq!(clock_text(0), "00:00");
        assert_eq!(clock_text(22 * 60 + 5), "22:05");
        assert_eq!(parse_clock("22:30"), Some(22 * 60 + 30));
        assert_eq!(parse_clock(" 7:05 "), Some(7 * 60 + 5));
        for bad in [
            "", "24:00", "7:60", "7", "7:", ":30", "ab:cd", "07:30:00", "-1:30",
        ] {
            assert_eq!(parse_clock(bad), None, "{bad:?}");
        }
        for minutes in [0u16, 59, 60, 1439] {
            assert_eq!(parse_clock(&clock_text(minutes)), Some(minutes));
        }
    }

    #[test]
    fn the_offset_is_read_from_the_zone_and_shown_in_words() {
        let phoenix = TimeZone::get("America/Phoenix").unwrap();
        let now = at(2026, 10, 28, 15, 0, &phoenix);
        assert_eq!(utc_offset_minutes(now, &phoenix), -7 * 60);
        assert_eq!(utc_offset_minutes(now, &TimeZone::UTC), 0);
        assert_eq!(offset_text(-420), "UTC−7");
        assert_eq!(offset_text(330), "UTC+5:30");
        assert_eq!(offset_text(0), "UTC");
    }

    #[test]
    fn quiet_hours_are_summarized_with_their_days() {
        let mut hours = QuietHours {
            start_minute: 22 * 60,
            end_minute: 7 * 60,
            utc_offset_minutes: -420,
            days: QuietHours::EVERY_DAY,
        };
        assert_eq!(quiet_summary(&hours), "22:00 to 07:00, every day");
        hours.days = 0b0001_1111;
        assert_eq!(
            quiet_summary(&hours),
            "22:00 to 07:00, Mon, Tue, Wed, Thu, Fri"
        );
    }

    #[test]
    fn a_pause_end_reads_as_a_time_tomorrow_a_weekday_or_a_date() {
        let zone = TimeZone::UTC;
        let now = at(2026, 10, 28, 15, 0, &zone);
        let label = |day: i8, hour: i8, minute: i8| {
            moment_label(at(2026, 10, day, hour, minute, &zone), now, &zone)
        };
        assert_eq!(label(28, 15, 30), "3:30 PM");
        assert_eq!(label(29, 9, 0), "tomorrow 9:00 AM");
        assert_eq!(label(31, 9, 0), "Saturday 9:00 AM");
        assert_eq!(
            moment_label(at(2026, 11, 20, 9, 0, &zone), now, &zone),
            "Nov 20 9:00 AM"
        );
    }

    #[test]
    fn the_pause_line_says_why_nothing_pops_up() {
        let zone = TimeZone::get("America/Phoenix").unwrap();
        let now = at(2026, 10, 29, 14, 0, &zone);
        let soon = at(2026, 10, 29, 15, 0, &zone);
        let years = at(2030, 1, 1, 0, 0, &zone);
        assert_eq!(pause_line(None, false, now, &zone), states::NOT_PAUSED);
        assert_eq!(
            pause_line(None, true, now, &zone),
            states::PAUSED_BY_QUIET_HOURS,
            "quiet hours alone are named, so the dialog agrees with the mark by the name"
        );
        assert!(
            pause_line(Some(soon), true, now, &zone).starts_with("Notifications are paused until")
        );
        assert_eq!(
            pause_line(Some(years), false, now, &zone),
            states::paused_until_resumed()
        );
    }
}
