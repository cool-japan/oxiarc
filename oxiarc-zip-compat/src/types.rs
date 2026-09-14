//! Shared value types: [`CompressionMethod`], [`DateTime`], [`System`].

use std::time::{SystemTime, UNIX_EPOCH};

use crate::result::DateTimeRangeError;

/// Identifies the storage format used to compress a file within a ZIP
/// archive.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Hash)]
#[non_exhaustive]
pub enum CompressionMethod {
    /// Store the file as is.
    Stored,
    /// Compress the file using Deflate.
    Deflated,
    /// Compress the file using Deflate64 (read-side identification only).
    Deflate64,
    /// Compress the file using BZIP2 (identification only).
    Bzip2,
    /// Encrypted using AES (identification only).
    Aes,
    /// Compress the file using ZStandard (identification only).
    Zstd,
    /// Compress the file using LZMA.
    Lzma,
    /// Compress the file using XZ (identification only).
    Xz,
    /// Compress the file using PPMd (identification only).
    Ppmd,
    /// Unsupported compression method.
    #[deprecated(since = "0.5.7", note = "use the constants instead")]
    Unsupported(u16),
}

#[allow(deprecated)]
impl CompressionMethod {
    /// The default compression method (Deflated).
    pub const DEFAULT: Self = CompressionMethod::Deflated;

    /// Converts a u16 to its corresponding CompressionMethod.
    pub const fn parse_from_u16(val: u16) -> Self {
        match val {
            0 => CompressionMethod::Stored,
            8 => CompressionMethod::Deflated,
            9 => CompressionMethod::Deflate64,
            12 => CompressionMethod::Bzip2,
            14 => CompressionMethod::Lzma,
            93 => CompressionMethod::Zstd,
            95 => CompressionMethod::Xz,
            98 => CompressionMethod::Ppmd,
            99 => CompressionMethod::Aes,
            v => CompressionMethod::Unsupported(v),
        }
    }

    /// Converts a u16 to its corresponding CompressionMethod.
    #[deprecated(since = "0.5.7", note = "use parse_from_u16 instead")]
    pub const fn from_u16(val: u16) -> CompressionMethod {
        Self::parse_from_u16(val)
    }

    /// Converts a CompressionMethod to a u16.
    pub const fn serialize_to_u16(self) -> u16 {
        match self {
            CompressionMethod::Stored => 0,
            CompressionMethod::Deflated => 8,
            CompressionMethod::Deflate64 => 9,
            CompressionMethod::Bzip2 => 12,
            CompressionMethod::Lzma => 14,
            CompressionMethod::Zstd => 93,
            CompressionMethod::Xz => 95,
            CompressionMethod::Ppmd => 98,
            CompressionMethod::Aes => 99,
            CompressionMethod::Unsupported(v) => v,
        }
    }

    /// Converts a CompressionMethod to a u16.
    #[deprecated(since = "0.5.7", note = "use serialize_to_u16 instead")]
    pub const fn to_u16(self) -> u16 {
        self.serialize_to_u16()
    }
}

impl Default for CompressionMethod {
    fn default() -> Self {
        CompressionMethod::DEFAULT
    }
}

impl std::fmt::Display for CompressionMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// The compression methods this implementation can write and read.
pub const SUPPORTED_COMPRESSION_METHODS: &[CompressionMethod] = &[
    CompressionMethod::Stored,
    CompressionMethod::Deflated,
    CompressionMethod::Lzma,
];

/// System inside the version-made-by field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
#[non_exhaustive]
pub enum System {
    /// MS-DOS.
    Dos = 0,
    /// Unix.
    #[default]
    Unix = 3,
    /// Anything else.
    Unknown,
}

/// Representation of a moment in time, with MS-DOS precision (two-second
/// resolution, 1980-2107).
#[derive(Clone, Copy, Eq, Hash, PartialEq, Debug)]
pub struct DateTime {
    datepart: u16,
    timepart: u16,
}

impl Default for DateTime {
    /// Constructs an 'default' datetime of 1980-01-01 00:00:00.
    fn default() -> DateTime {
        DateTime::DEFAULT
    }
}

impl DateTime {
    /// The earliest possible date: 1980-01-01 00:00:00.
    pub const DEFAULT: Self = DateTime {
        datepart: 0b0000_0000_0010_0001,
        timepart: 0,
    };

    /// Converts an msdos (u16, u16) pair to a DateTime object.
    ///
    /// # Safety
    ///
    /// The caller must ensure the date and time are valid (kept `unsafe`
    /// only for signature compatibility; nothing unsound happens either
    /// way).
    pub const unsafe fn from_msdos_unchecked(datepart: u16, timepart: u16) -> DateTime {
        DateTime { datepart, timepart }
    }

    /// Converts an msdos (u16, u16) pair to a DateTime object if it
    /// represents a valid date and time.
    ///
    /// # Errors
    ///
    /// [`DateTimeRangeError`] for an out-of-range field.
    pub fn try_from_msdos(datepart: u16, timepart: u16) -> Result<DateTime, DateTimeRangeError> {
        let seconds = (timepart & 0b0000_0000_0001_1111) << 1;
        let minutes = (timepart & 0b0000_0111_1110_0000) >> 5;
        let hours = (timepart & 0b1111_1000_0000_0000) >> 11;
        let days = datepart & 0b0000_0000_0001_1111;
        let months = (datepart & 0b0000_0001_1110_0000) >> 5;
        let years = (datepart & 0b1111_1110_0000_0000) >> 9;
        Self::from_date_and_time(
            years + 1980,
            months as u8,
            days as u8,
            hours as u8,
            minutes as u8,
            seconds as u8,
        )
    }

    /// Constructs a DateTime from a specific date and time.
    ///
    /// # Errors
    ///
    /// [`DateTimeRangeError`] when a field is outside the MS-DOS range.
    pub fn from_date_and_time(
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    ) -> Result<DateTime, DateTimeRangeError> {
        let max_day = days_in_month(year, month);
        if (1980..=2107).contains(&year)
            && (1..=12).contains(&month)
            && day >= 1
            && day <= max_day
            && hour <= 23
            && minute <= 59
            && second <= 60
        {
            let second = second.min(58);
            let datepart = (day as u16) | ((month as u16) << 5) | ((year - 1980) << 9);
            let timepart = ((second as u16) >> 1) | ((minute as u16) << 5) | ((hour as u16) << 11);
            Ok(DateTime { datepart, timepart })
        } else {
            Err(DateTimeRangeError)
        }
    }

    /// The current time (UTC), clamped to the MS-DOS range: what `zip`
    /// uses as the default modification time when writing.
    pub fn default_for_write() -> Self {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self::from_unix_seconds(secs)
    }

    /// Build from Unix seconds (UTC), clamped to 1980-01-01..=2107-12-31.
    pub(crate) fn from_unix_seconds(secs: u64) -> Self {
        let days = (secs / 86_400) as i64;
        let rem = secs % 86_400;
        let (year, month, day) = civil_from_days(days);
        let hour = (rem / 3600) as u8;
        let minute = ((rem % 3600) / 60) as u8;
        let second = (rem % 60) as u8;
        if year < 1980 {
            return DateTime::DEFAULT;
        }
        if year > 2107 {
            return DateTime {
                datepart: (31) | (12 << 5) | (127 << 9),
                timepart: (29) | (59 << 5) | (23 << 11),
            };
        }
        DateTime::from_date_and_time(year as u16, month, day, hour, minute, second)
            .unwrap_or(DateTime::DEFAULT)
    }

    /// Gets the time portion of this datetime in the msdos representation.
    pub const fn timepart(&self) -> u16 {
        self.timepart
    }

    /// Gets the date portion of this datetime in the msdos representation.
    pub const fn datepart(&self) -> u16 {
        self.datepart
    }

    /// Get the year. There is no epoch, i.e. 2018 will be returned as 2018.
    pub const fn year(&self) -> u16 {
        (self.datepart >> 9) + 1980
    }

    /// Get the month, where 1 = january and 12 = december.
    pub const fn month(&self) -> u8 {
        ((self.datepart & 0b0000_0001_1110_0000) >> 5) as u8
    }

    /// Get the day.
    pub const fn day(&self) -> u8 {
        (self.datepart & 0b0000_0000_0001_1111) as u8
    }

    /// Get the hour.
    pub const fn hour(&self) -> u8 {
        (self.timepart >> 11) as u8
    }

    /// Get the minute.
    pub const fn minute(&self) -> u8 {
        ((self.timepart & 0b0000_0111_1110_0000) >> 5) as u8
    }

    /// Get the second (even values only; MS-DOS has 2-second resolution).
    pub const fn second(&self) -> u8 {
        ((self.timepart & 0b0000_0000_0001_1111) << 1) as u8
    }

    /// Returns whether this is a valid date and time.
    pub fn is_valid(&self) -> bool {
        DateTime::try_from_msdos(self.datepart, self.timepart).is_ok()
    }
}

fn is_leap(year: u16) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u8, u8) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_roundtrip() {
        let dt = DateTime::from_date_and_time(2024, 2, 29, 13, 45, 31);
        let Ok(dt) = dt else {
            panic!("valid date rejected");
        };
        assert_eq!(
            (
                dt.year(),
                dt.month(),
                dt.day(),
                dt.hour(),
                dt.minute(),
                dt.second()
            ),
            (2024, 2, 29, 13, 45, 30)
        );
        assert!(DateTime::from_date_and_time(2023, 2, 29, 0, 0, 0).is_err());
        assert!(DateTime::from_date_and_time(1979, 1, 1, 0, 0, 0).is_err());
        let unix = DateTime::from_unix_seconds(1_700_000_000);
        assert_eq!((unix.year(), unix.month(), unix.day()), (2023, 11, 14));
        assert_eq!((unix.hour(), unix.minute(), unix.second()), (22, 13, 20));
    }
}
