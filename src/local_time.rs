//! The local time as a calendar date and clock, for what shows or writes
//! one (Local History's days, snippets' `CURRENT_*` variables) without a
//! date crate.

use std::time::SystemTime;

/// The local time zone's offset from UTC, in seconds (0 where it can't be read).
pub fn offset() -> i64 {
    #[cfg(unix)]
    {
        let now = unsafe { libc::time(std::ptr::null_mut()) };
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        if unsafe { !libc::localtime_r(&now, &mut tm).is_null() } {
            return tm.tm_gmtoff;
        }
    }
    0
}

/// Local time as (days since the epoch, seconds into the day).
pub fn day_and_seconds(time: SystemTime) -> (i64, i64) {
    let secs = time.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64) + offset();
    (secs.div_euclid(86_400), secs.rem_euclid(86_400))
}

/// Days since the epoch to (year, month 1–12, day 1–31) (Howard Hinnant's algorithm).
pub fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m as u32, d as u32)
}

/// A moment in local time.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LocalTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 0 = Monday … 6 = Sunday.
    pub weekday: u32,
    /// Seconds since the epoch (UTC).
    pub unix: i64,
}

impl LocalTime {
    pub fn now() -> Self {
        Self::at(SystemTime::now())
    }

    pub fn at(time: SystemTime) -> Self {
        let (days, secs) = day_and_seconds(time);
        let (year, month, day) = civil(days);
        LocalTime {
            year,
            month,
            day,
            hour: (secs / 3600) as u32,
            minute: (secs % 3600 / 60) as u32,
            second: (secs % 60) as u32,
            // 1970-01-01 was a Thursday.
            weekday: (days + 3).rem_euclid(7) as u32,
            unix: time.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20_405), (2025, 11, 13));
        assert_eq!(civil(-1), (1969, 12, 31));
        assert_eq!(civil(11_016), (2000, 2, 29));
    }
}
