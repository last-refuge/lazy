//! Times as the person reads them: in their own time zone, "10:42 AM" or "10:42" as their locale
//! writes it, and days as "Today", "Yesterday", or "Monday, November 2".
//!
//! The browser knows the time zone (with its daylight-saving changes) and the locale's clock; the
//! [`Clock`] trait asks it, so the views can be tested with a fixed one.

use lazy_domain::Timestamp;

/// A moment in the person's time zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Local {
    pub year: i32,
    /// 1 to 12.
    pub month: u8,
    pub day: u8,
    /// 0 is Sunday.
    pub weekday: u8,
    pub hour: u8,
    pub minute: u8,
}

impl Local {
    /// The day, for telling days apart.
    pub fn day_key(&self) -> (i32, u8, u8) {
        (self.year, self.month, self.day)
    }
}

pub(crate) trait Clock {
    fn local(&self, at: Timestamp) -> Local;
    fn now(&self) -> Timestamp;
    /// The locale writes times with AM and PM.
    fn hour12(&self) -> bool;
}

/// The browser's clock and time zone.
pub(crate) struct BrowserClock {
    hour12: bool,
}

impl BrowserClock {
    pub(crate) fn new() -> BrowserClock {
        // A time the locale writes with "AM" or "PM" (or its own words for them) is longer than
        // "1:00": ask how 13:00 is written.
        let date = js_sys::Date::new_with_year_month_day_hr_min(2020, 0, 1, 13, 0);
        let text: String = date
            .to_locale_time_string_with_options(
                "default",
                &js_sys::Object::from_entries(&js_sys::Array::of2(
                    &js_sys::Array::of2(&"hour".into(), &"numeric".into()),
                    &js_sys::Array::of2(&"minute".into(), &"2-digit".into()),
                ))
                .unwrap_or_else(|_| js_sys::Object::new()),
            )
            .into();
        BrowserClock {
            hour12: !text.contains("13"),
        }
    }
}

impl Clock for BrowserClock {
    fn local(&self, at: Timestamp) -> Local {
        let millis = (at.as_offset_date_time().unix_timestamp_nanos() / 1_000_000) as f64;
        let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(millis));
        Local {
            year: date.get_full_year() as i32,
            month: (date.get_month() + 1) as u8,
            day: date.get_date() as u8,
            weekday: date.get_day() as u8,
            hour: date.get_hours() as u8,
            minute: date.get_minutes() as u8,
        }
    }

    fn now(&self) -> Timestamp {
        Timestamp::now()
    }

    fn hour12(&self) -> bool {
        self.hour12
    }
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

/// "10:42 AM", or "10:42" on a 24-hour clock.
pub(crate) fn time_of_day(clock: &dyn Clock, at: Timestamp) -> String {
    let local = clock.local(at);
    if clock.hour12() {
        let (hour, half) = match local.hour {
            0 => (12, "AM"),
            1..=11 => (local.hour, "AM"),
            12 => (12, "PM"),
            h => (h - 12, "PM"),
        };
        format!("{hour}:{:02} {half}", local.minute)
    } else {
        format!("{}:{:02}", local.hour, local.minute)
    }
}

/// "Today", "Yesterday", "Monday, November 2", or with the year when it is not this year.
pub(crate) fn day_label(clock: &dyn Clock, at: Timestamp) -> String {
    let local = clock.local(at);
    let now = clock.now();
    let today = clock.local(now);
    let yesterday = clock.local(Timestamp::from_offset_date_time(
        now.as_offset_date_time() - time::Duration::days(1),
    ));
    if local.day_key() == today.day_key() {
        return "Today".to_owned();
    }
    if local.day_key() == yesterday.day_key() {
        return "Yesterday".to_owned();
    }
    let weekday = WEEKDAYS[usize::from(local.weekday % 7)];
    let month = MONTHS[usize::from(local.month.clamp(1, 12) - 1)];
    if local.year == today.year {
        format!("{weekday}, {month} {}", local.day)
    } else {
        format!("{weekday}, {month} {}, {}", local.day, local.year)
    }
}

/// A clock in UTC at a fixed moment, for tests.
#[cfg(test)]
pub(crate) struct FixedClock {
    pub now: Timestamp,
    pub hour12: bool,
}

#[cfg(test)]
impl Clock for FixedClock {
    fn local(&self, at: Timestamp) -> Local {
        let t = at.as_offset_date_time();
        Local {
            year: t.year(),
            month: u8::from(t.month()),
            day: t.day(),
            weekday: t.weekday().number_days_from_sunday(),
            hour: t.hour(),
            minute: t.minute(),
        }
    }

    fn now(&self) -> Timestamp {
        self.now
    }

    fn hour12(&self) -> bool {
        self.hour12
    }
}

#[cfg(test)]
pub(crate) fn at(text: &str) -> Timestamp {
    Timestamp::from_offset_date_time(
        time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339).unwrap(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_days_read_naturally() {
        let clock = FixedClock {
            now: at("2026-11-02T15:00:00Z"),
            hour12: true,
        };
        assert_eq!(time_of_day(&clock, at("2026-11-02T00:05:00Z")), "12:05 AM");
        assert_eq!(time_of_day(&clock, at("2026-11-02T13:40:00Z")), "1:40 PM");
        assert_eq!(day_label(&clock, at("2026-11-02T01:00:00Z")), "Today");
        assert_eq!(day_label(&clock, at("2026-11-01T23:00:00Z")), "Yesterday");
        assert_eq!(
            day_label(&clock, at("2026-10-28T10:00:00Z")),
            "Wednesday, October 28"
        );
        assert_eq!(
            day_label(&clock, at("2025-12-31T10:00:00Z")),
            "Wednesday, December 31, 2025"
        );
        let clock = FixedClock {
            now: at("2026-11-02T15:00:00Z"),
            hour12: false,
        };
        assert_eq!(time_of_day(&clock, at("2026-11-02T13:40:00Z")), "13:40");
    }
}
