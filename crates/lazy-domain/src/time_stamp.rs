use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use time::OffsetDateTime;

/// A point in time, serialized as an RFC 3339 string.
///
/// Timestamps are for display. Ordering and replay use [`crate::EventSeq`], never timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(OffsetDateTime);

impl Timestamp {
    pub fn now() -> Self {
        Self(OffsetDateTime::now_utc())
    }

    pub const fn from_offset_date_time(value: OffsetDateTime) -> Self {
        Self(value)
    }

    pub const fn as_offset_date_time(&self) -> OffsetDateTime {
        self.0
    }
}

impl From<OffsetDateTime> for Timestamp {
    fn from(value: OffsetDateTime) -> Self {
        Self(value)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self
            .0
            .format(&time::format_description::well_known::Rfc3339)
        {
            Ok(text) => f.write_str(&text),
            Err(_) => Err(fmt::Error),
        }
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        time::serde::rfc3339::serialize(&self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        time::serde::rfc3339::deserialize(deserializer).map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_as_rfc3339() {
        let stamp = Timestamp::from(OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap());
        let json = serde_json::to_string(&stamp).unwrap();
        assert_eq!(json, "\"2027-01-15T08:00:00Z\"");
        assert_eq!(serde_json::from_str::<Timestamp>(&json).unwrap(), stamp);
    }
}
