use time::format_description::FormatItem;
use time::macros::format_description;
use time::OffsetDateTime;

const FALLBACK_DATE_TIME_FORMAT: &[FormatItem<'_>] =
    format_description!("[hour]:[minute] [month]/[day]/[year]");

pub(crate) fn time_date_for_insert(now: OffsetDateTime) -> String {
    os_short_time_date(now).unwrap_or_else(|| fallback_time_date(now))
}

fn fallback_time_date(now: OffsetDateTime) -> String {
    now.format(FALLBACK_DATE_TIME_FORMAT)
        .unwrap_or_else(|_| "00:00 1/1/1970".to_owned())
}

#[cfg(target_os = "windows")]
fn os_short_time_date(now: OffsetDateTime) -> Option<String> {
    windows_short_format::short_time_date(now)
}

#[cfg(not(target_os = "windows"))]
fn os_short_time_date(_now: OffsetDateTime) -> Option<String> {
    None
}

#[cfg(target_os = "windows")]
mod windows_short_format {
    use std::ptr::{null, null_mut};

    use time::OffsetDateTime;
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::Globalization::{
        GetDateFormatEx, GetLocaleInfoEx, GetTimeFormatEx, LOCALE_SSHORTDATE, LOCALE_SSHORTTIME,
    };

    pub(super) fn short_time_date(now: OffsetDateTime) -> Option<String> {
        let date_pattern = locale_pattern(LOCALE_SSHORTDATE)?;
        let time_pattern = locale_pattern(LOCALE_SSHORTTIME)?;
        short_time_date_with_wide_patterns(now, &date_pattern, &time_pattern)
    }

    #[cfg(test)]
    pub(super) fn short_time_date_with_patterns(
        now: OffsetDateTime,
        date_pattern: &str,
        time_pattern: &str,
    ) -> Option<String> {
        let date_pattern = wide_null(date_pattern);
        let time_pattern = wide_null(time_pattern);
        short_time_date_with_wide_patterns(now, &date_pattern, &time_pattern)
    }

    fn short_time_date_with_wide_patterns(
        now: OffsetDateTime,
        date_pattern: &[u16],
        time_pattern: &[u16],
    ) -> Option<String> {
        let system_time = system_time(now);
        let time = format_time(&system_time, time_pattern)?;
        let date = format_date(&system_time, date_pattern)?;
        Some(format!("{time} {date}"))
    }

    fn locale_pattern(locale_type: u32) -> Option<Vec<u16>> {
        let required = unsafe { GetLocaleInfoEx(null(), locale_type, null_mut(), 0) };
        if required <= 1 {
            return None;
        }

        let mut buffer = vec![0; required as usize];
        let written = unsafe {
            GetLocaleInfoEx(
                null(),
                locale_type,
                buffer.as_mut_ptr(),
                buffer.len() as i32,
            )
        };
        if written <= 1 {
            return None;
        }

        buffer.truncate(written as usize);
        Some(buffer)
    }

    fn format_date(system_time: &SYSTEMTIME, pattern: &[u16]) -> Option<String> {
        let required = unsafe {
            GetDateFormatEx(
                null(),
                0,
                system_time,
                pattern.as_ptr(),
                null_mut(),
                0,
                null(),
            )
        };
        if required <= 1 {
            return None;
        }

        let mut buffer = vec![0; required as usize];
        let written = unsafe {
            GetDateFormatEx(
                null(),
                0,
                system_time,
                pattern.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len() as i32,
                null(),
            )
        };
        utf16_output(&buffer, written)
    }

    fn format_time(system_time: &SYSTEMTIME, pattern: &[u16]) -> Option<String> {
        let required =
            unsafe { GetTimeFormatEx(null(), 0, system_time, pattern.as_ptr(), null_mut(), 0) };
        if required <= 1 {
            return None;
        }

        let mut buffer = vec![0; required as usize];
        let written = unsafe {
            GetTimeFormatEx(
                null(),
                0,
                system_time,
                pattern.as_ptr(),
                buffer.as_mut_ptr(),
                buffer.len() as i32,
            )
        };
        utf16_output(&buffer, written)
    }

    fn utf16_output(buffer: &[u16], written: i32) -> Option<String> {
        if written <= 1 {
            return None;
        }
        String::from_utf16(&buffer[..written as usize - 1]).ok()
    }

    fn system_time(now: OffsetDateTime) -> SYSTEMTIME {
        SYSTEMTIME {
            wYear: now.year() as u16,
            wMonth: u8::from(now.month()) as u16,
            wDayOfWeek: 0,
            wDay: now.day() as u16,
            wHour: now.hour() as u16,
            wMinute: now.minute() as u16,
            wSecond: now.second() as u16,
            wMilliseconds: now.millisecond(),
        }
    }

    #[cfg(test)]
    fn wide_null(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::*;

    #[test]
    fn fallback_time_date_preserves_time_then_date_order() {
        let formatted = fallback_time_date(datetime!(2026-05-28 09:05:07 UTC));

        assert!(formatted.starts_with("09:05 "));
        assert!(formatted.ends_with("/2026"));
    }

    #[test]
    fn time_date_for_insert_produces_text() {
        let formatted = time_date_for_insert(datetime!(2026-05-28 09:05:07 UTC));

        assert!(!formatted.is_empty());
        assert!(formatted.contains(' '));
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn windows_short_time_date_uses_supplied_short_patterns() {
        let formatted = windows_short_format::short_time_date_with_patterns(
            datetime!(2026-05-28 21:07:09 UTC),
            "yyyy/MM/dd",
            "HH:mm",
        )
        .expect("windows short date/time formatting");

        assert_eq!(formatted, "21:07 2026/05/28");
    }
}
