//! Append-mode file opening and UTC timestamp formatting for the journal.
//!
//! These were the only live parts of the former `daemon::event_log` module,
//! which existed to host an `EventLogger` that no production code ever
//! constructed (#1165 finding 7). They live here now because the compile
//! journal is their only consumer.

use std::fs::File;
use std::path::Path;
use std::time::SystemTime;

/// Open a file in append mode with sharing flags that allow deletion on Windows.
///
/// On Windows, Rust's default `OpenOptions` uses `FILE_SHARE_READ | FILE_SHARE_WRITE`
/// but omits `FILE_SHARE_DELETE`, which prevents any other process from deleting or
/// renaming the file while a handle is open. This helper adds `FILE_SHARE_DELETE`
/// so log files remain deletable at any time.
pub(crate) fn open_append(path: &Path) -> std::io::Result<File> {
    crate::platform::fs::durability::open_shared_append(path)
}

/// Format a `SystemTime` as `YYYY-MM-DDTHH:MM:SS.mmmZ` in UTC.
///
/// Manual decomposition from Unix epoch — no external dependency needed.
pub(crate) fn format_timestamp(time: SystemTime) -> String {
    let dur = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let total_secs = dur.as_secs();
    let millis = dur.subsec_millis();

    let days = total_secs / 86400;
    let day_secs = total_secs % 86400;
    let hour = day_secs / 3600;
    let minute = (day_secs % 3600) / 60;
    let second = day_secs % 60;

    let (year, month, day) = civil_from_days(days as i64);

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

/// Convert days since 1970-01-01 to (year, month, day).
/// Howard Hinnant's algorithm — public domain.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_format_timestamp() {
        let time = SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600);
        let ts = format_timestamp(time);
        assert_eq!(ts, "2026-01-01T00:00:00.000Z");
    }
}
