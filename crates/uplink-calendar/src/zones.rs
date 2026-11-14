//! Time zones as Graph takes an event's times in (SPEC §10.4): by
//! Windows' names, which CLDR maps IANA's to. A client gives its own
//! (IANA's), in which a series recurs, across daylight saving time.

use anyhow::{Context, Result};

mod table {
    include!("windows_zones.rs");
}

/// A time zone a client named, as Graph knows it too.
#[derive(Debug, Clone)]
pub struct Zone {
    zone: jiff::tz::TimeZone,
    /// Its Windows name.
    pub windows: &'static str,
}

impl Zone {
    /// The zone of IANA's `name`; an error if jiff or Windows knows none.
    pub fn new(name: &str) -> Result<Self> {
        let zone = jiff::tz::db()
            .get(name)
            .with_context(|| format!("{name} is not a time zone"))?;
        let windows = table::WINDOWS_ZONES
            .binary_search_by(|(iana, _)| (*iana).cmp(name))
            .map(|i| table::WINDOWS_ZONES[i].1)
            .ok()
            .with_context(|| format!("{name} has no Windows time zone"))?;
        Ok(Self { zone, windows })
    }

    /// The wall time `ms` (since the epoch) is here, `YYYY-MM-DDTHH:MM:SS`.
    pub fn wall(&self, ms: i64) -> String {
        self.zoned(ms).strftime("%Y-%m-%dT%H:%M:%S").to_string()
    }

    /// The date `ms` is on here, `YYYY-MM-DD`.
    pub fn date(&self, ms: i64) -> String {
        self.zoned(ms).strftime("%Y-%m-%d").to_string()
    }

    fn zoned(&self, ms: i64) -> jiff::Zoned {
        jiff::Timestamp::from_millisecond(ms)
            .expect("a time the calendar holds")
            .to_zoned(self.zone.clone())
    }
}

/// The UTC date of a UTC midnight, as an all-day event's days are.
pub fn utc_date(ms: i64) -> String {
    jiff::Timestamp::from_millisecond(ms)
        .expect("a time the calendar holds")
        .strftime("%Y-%m-%d")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_windows_zones_and_tells_wall_times() {
        let prague = Zone::new("Europe/Prague").unwrap();
        assert_eq!(prague.windows, "Central Europe Standard Time");
        // 2026-10-26 09:00Z, after daylight saving time ended.
        assert_eq!(prague.wall(1_793_005_200_000), "2026-10-26T10:00:00");
        assert_eq!(prague.date(1_793_005_200_000), "2026-10-26");
        // Either of tzdb's names.
        assert_eq!(
            Zone::new("Asia/Kolkata").unwrap().windows,
            "India Standard Time"
        );
        assert_eq!(
            Zone::new("Asia/Calcutta").unwrap().windows,
            "India Standard Time"
        );
        assert_eq!(Zone::new("UTC").unwrap().windows, "UTC");
        assert!(Zone::new("Mars/Olympus").is_err());
        assert_eq!(utc_date(1_793_059_200_000), "2026-10-27");
    }
}
