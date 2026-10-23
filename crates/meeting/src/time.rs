//! Local time for meeting ids, titles and dates, the way `agt-history` does it: the zone's
//! offset from the OS (`localtime_r`, Win32), the calendar maths by hand.

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A moment in local time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct LocalTime {
    /// Days since the epoch (local).
    days: i64,
    /// Milliseconds into the (local) day.
    ms: i64,
}

impl LocalTime {
    pub(crate) fn at(unix_ms: i64) -> Self {
        Self::with_offset(unix_ms, utc_offset(unix_ms.div_euclid(1000)))
    }

    /// `offset`: seconds east of UTC.
    pub(crate) fn with_offset(unix_ms: i64, offset: i64) -> Self {
        let local = unix_ms + offset * 1000;
        Self {
            days: local.div_euclid(86_400_000),
            ms: local.rem_euclid(86_400_000),
        }
    }

    /// A meeting's id: `YYYY-MM-DD-HHMMSS`.
    pub(crate) fn id(&self) -> String {
        let (y, mo, d) = civil_from_days(self.days);
        let (h, mi, s) = self.clock();
        format!("{y:04}-{mo:02}-{d:02}-{h:02}{mi:02}{s:02}")
    }

    /// `Oct 16, 2:30 PM`.
    pub(crate) fn short(&self) -> String {
        let (_, mo, d) = civil_from_days(self.days);
        format!("{} {d}, {}", MONTHS[(mo - 1) as usize], self.time_of_day())
    }

    /// `Oct 16, 2026, 2:30 PM`.
    pub(crate) fn long(&self) -> String {
        let (y, mo, d) = civil_from_days(self.days);
        format!(
            "{} {d}, {y}, {}",
            MONTHS[(mo - 1) as usize],
            self.time_of_day()
        )
    }

    /// `2:30 PM`, `12:05 AM`.
    fn time_of_day(&self) -> String {
        let (h, mi, _) = self.clock();
        let half = if h < 12 { "AM" } else { "PM" };
        let h12 = if h % 12 == 0 { 12 } else { h % 12 };
        format!("{h12}:{mi:02} {half}")
    }

    fn clock(&self) -> (i64, i64, i64) {
        let secs = self.ms / 1000;
        (secs / 3600, secs / 60 % 60, secs % 60)
    }
}

/// A point on the meeting clock: `mm:ss`, or `h:mm:ss` from an hour on.
pub(crate) fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// How long a meeting ran, rounded up to the minute: `45 min`, `1 h 5 min`.
pub(crate) fn duration(ms: i64) -> String {
    let min = (ms.max(0) as u64).div_ceil(60_000).max(1);
    match (min / 60, min % 60) {
        (0, m) => format!("{m} min"),
        (h, 0) => format!("{h} h"),
        (h, m) => format!("{h} h {m} min"),
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
#[cfg(any(windows, test))]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of `days_from_civil`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + (m <= 2) as i64, m, d)
}

/// The local time zone's offset from UTC at `unix_secs`, in seconds (DST included).
#[cfg(windows)]
fn utc_offset(unix_secs: i64) -> i64 {
    use windows_sys::Win32::{
        Foundation::SYSTEMTIME, System::Time::SystemTimeToTzSpecificLocalTime,
    };
    let (y, m, d) = civil_from_days(unix_secs.div_euclid(86_400));
    let sod = unix_secs.rem_euclid(86_400);
    let utc = SYSTEMTIME {
        wYear: y as u16,
        wMonth: m as u16,
        wDayOfWeek: 0,
        wDay: d as u16,
        wHour: (sod / 3600) as u16,
        wMinute: (sod / 60 % 60) as u16,
        wSecond: (sod % 60) as u16,
        wMilliseconds: 0,
    };
    let mut local = utc;
    // SAFETY: both pointers are to valid SYSTEMTIMEs; null means the current time zone.
    if unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) } == 0 {
        return 0;
    }
    let days = days_from_civil(local.wYear.into(), local.wMonth.into(), local.wDay.into());
    let secs = i64::from(local.wHour) * 3600 + i64::from(local.wMinute) * 60;
    days * 86_400 + secs + i64::from(local.wSecond) - unix_secs
}

#[cfg(unix)]
fn utc_offset(unix_secs: i64) -> i64 {
    let t = unix_secs as libc::time_t;
    // SAFETY: `tm` is plain data, filled in by localtime_r.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff as i64
}

#[cfg(not(any(windows, unix)))]
fn utc_offset(_: i64) -> i64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-16 12:30:05 UTC.
    const MS: i64 = 1_792_153_805_000;

    #[test]
    fn formats() {
        assert_eq!(
            MS,
            days_from_civil(2026, 10, 16) * 86_400_000 + (12 * 3600 + 30 * 60 + 5) * 1000
        );
        // Two hours east.
        let t = LocalTime::with_offset(MS, 7200);
        assert_eq!(t.id(), "2026-10-16-143005");
        assert_eq!(t.short(), "Oct 16, 2:30 PM");
        assert_eq!(t.long(), "Oct 16, 2026, 2:30 PM");
        // West, across midnight: 00:00:05 on the 16th, then 23:00:05 on the 15th.
        assert_eq!(
            LocalTime::with_offset(MS, -(12 * 3600 + 30 * 60)).short(),
            "Oct 16, 12:00 AM"
        );
        assert_eq!(
            LocalTime::with_offset(MS, -(13 * 3600 + 30 * 60)).id(),
            "2026-10-15-230005"
        );
        assert_eq!(
            LocalTime::with_offset(MS, -30 * 60).short(),
            "Oct 16, 12:00 PM"
        );
        assert_eq!(
            LocalTime::with_offset(MS, -(11 * 3600 + 30 * 60)).short(),
            "Oct 16, 1:00 AM"
        );
        // Whatever this machine's zone, it's within a day of UTC and whole minutes.
        let off = utc_offset(MS / 1000);
        assert!(off.abs() <= 14 * 3600 && off % 60 == 0, "{off}");
    }

    #[test]
    fn meeting_clock() {
        assert_eq!(clock(0), "00:00");
        assert_eq!(clock(5_999), "00:05");
        assert_eq!(clock(59 * 60_000 + 59_000), "59:59");
        assert_eq!(clock(3_600_000 + 2 * 60_000 + 3_000), "1:02:03");
        assert_eq!(duration(0), "1 min");
        assert_eq!(duration(45 * 60_000 - 10_000), "45 min");
        assert_eq!(duration(60 * 60_000), "1 h");
        assert_eq!(duration(65 * 60_000), "1 h 5 min");
    }
}
