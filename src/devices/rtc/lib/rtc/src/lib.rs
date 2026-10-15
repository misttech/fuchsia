// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_next_fuchsia_hardware_rtc as frtc;
use log::warn;

// Time conversion constants (same as pl031-rtc)
pub const LOCAL_EPOCH: u64 = 946684800;
pub const LOCAL_EPOCH_YEAR: u16 = 2000;
pub const DEFAULT_YEAR: u16 = 2020;
pub const MAX_YEAR: u16 = 2099;

pub const DAYS_IN_MONTH: [u64; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

pub const DEFAULT_RTC: frtc::Time = frtc::Time {
    seconds: 0,
    minutes: 0,
    hours: 0,
    day: 1,
    month: 1, // JANUARY
    year: DEFAULT_YEAR,
};

pub fn is_leap_year(year: u16) -> bool {
    ((year % 4) == 0 && (year % 100) != 0) || ((year % 400) == 0)
}

pub fn days_in_year(year: u16) -> u64 {
    if is_leap_year(year) { 366 } else { 365 }
}

pub fn days_in_month(month: u8, year: u16) -> u64 {
    let mut days = DAYS_IN_MONTH[month as usize];
    if month == 2 && is_leap_year(year) {
        days += 1;
    }
    days
}

pub fn is_rtc_valid(rtc: &frtc::Time) -> bool {
    if rtc.year < LOCAL_EPOCH_YEAR || rtc.year > MAX_YEAR {
        return false;
    }
    if rtc.month < 1 || rtc.month > 12 {
        return false;
    }
    if rtc.day as u64 > days_in_month(rtc.month, rtc.year) {
        return false;
    }
    if rtc.day == 0 {
        return false;
    }
    if rtc.hours > 23 || rtc.minutes > 59 || rtc.seconds > 59 {
        return false;
    }
    true
}

pub fn seconds_to_rtc(seconds: u64) -> frtc::Time {
    if seconds < LOCAL_EPOCH {
        warn!("SecondsToRtc: Seconds value is out of range, returning default");
        return DEFAULT_RTC;
    }

    let mut epoch = seconds - LOCAL_EPOCH;
    let mut rtc = frtc::Time {
        seconds: (epoch % 60) as u8,
        minutes: 0,
        hours: 0,
        day: 0,
        month: 0,
        year: LOCAL_EPOCH_YEAR,
    };
    epoch /= 60;
    rtc.minutes = (epoch % 60) as u8;
    epoch /= 60;
    rtc.hours = (epoch % 24) as u8;
    epoch /= 24;

    for year in LOCAL_EPOCH_YEAR.. {
        let days = days_in_year(year);
        if epoch < days {
            rtc.year = year;
            break;
        }
        epoch -= days;
    }

    for month in 1..=12 {
        let days = days_in_month(month, rtc.year);
        if epoch < days {
            rtc.month = month;
            break;
        }
        epoch -= days;
    }

    rtc.day = (epoch + 1) as u8;
    rtc
}

pub fn seconds_since_epoch(rtc: &frtc::Time) -> u64 {
    let mut days_since_local_epoch = 0;
    for year in LOCAL_EPOCH_YEAR..rtc.year {
        days_since_local_epoch += days_in_year(year);
    }
    for month in 1..rtc.month {
        days_since_local_epoch += days_in_month(month, rtc.year);
    }
    days_since_local_epoch += (rtc.day - 1) as u64;

    let hours = (days_since_local_epoch * 24) + rtc.hours as u64;
    let minutes = (hours * 60) + rtc.minutes as u64;
    let seconds = (minutes * 60) + rtc.seconds as u64;

    LOCAL_EPOCH + seconds
}

pub fn sanitize_rtc(rtc: frtc::Time) -> frtc::Time {
    if !is_rtc_valid(&rtc) || rtc.year < DEFAULT_YEAR {
        warn!("RTC is sanitized to constant default");
        return DEFAULT_RTC;
    }
    rtc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_leap_year() {
        assert!(is_leap_year(2000));
        assert!(is_leap_year(2020));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(2021));
        assert!(!is_leap_year(2022));
        assert!(!is_leap_year(2023));
        assert!(!is_leap_year(2100));
    }

    #[test]
    fn test_is_rtc_valid() {
        let valid =
            frtc::Time { seconds: 30, minutes: 15, hours: 10, day: 15, month: 6, year: 2024 };
        assert!(is_rtc_valid(&valid));

        let invalid_year = frtc::Time { year: 1999, ..valid };
        assert!(!is_rtc_valid(&invalid_year));

        let invalid_year_high = frtc::Time { year: 2100, ..valid };
        assert!(!is_rtc_valid(&invalid_year_high));

        let invalid_month = frtc::Time { month: 13, ..valid };
        assert!(!is_rtc_valid(&invalid_month));

        let invalid_day_zero = frtc::Time { day: 0, ..valid };
        assert!(!is_rtc_valid(&invalid_day_zero));

        let invalid_feb = frtc::Time { day: 29, month: 2, year: 2023, ..valid };
        assert!(!is_rtc_valid(&invalid_feb));

        let valid_feb = frtc::Time { day: 29, month: 2, year: 2024, ..valid };
        assert!(is_rtc_valid(&valid_feb));

        let invalid_hours = frtc::Time { hours: 24, ..valid };
        assert!(!is_rtc_valid(&invalid_hours));

        let invalid_minutes = frtc::Time { minutes: 60, ..valid };
        assert!(!is_rtc_valid(&invalid_minutes));

        let invalid_seconds = frtc::Time { seconds: 60, ..valid };
        assert!(!is_rtc_valid(&invalid_seconds));
    }

    #[test]
    fn test_seconds_conversion_roundtrip() {
        let time =
            frtc::Time { seconds: 45, minutes: 30, hours: 14, day: 28, month: 2, year: 2024 };
        let seconds = seconds_since_epoch(&time);
        let converted = seconds_to_rtc(seconds);
        assert_eq!(converted.seconds, time.seconds);
        assert_eq!(converted.minutes, time.minutes);
        assert_eq!(converted.hours, time.hours);
        assert_eq!(converted.day, time.day);
        assert_eq!(converted.month, time.month);
        assert_eq!(converted.year, time.year);
    }

    #[test]
    fn test_sanitize_rtc() {
        let invalid = frtc::Time { seconds: 0, minutes: 0, hours: 0, day: 1, month: 1, year: 2010 };
        let sanitized = sanitize_rtc(invalid);
        assert_eq!(sanitized.year, DEFAULT_YEAR);
        assert_eq!(sanitized.month, 1);
        assert_eq!(sanitized.day, 1);

        let valid =
            frtc::Time { seconds: 10, minutes: 20, hours: 8, day: 14, month: 10, year: 2026 };
        let kept = sanitize_rtc(valid);
        assert_eq!(kept.year, 2026);
        assert_eq!(kept.month, 10);
        assert_eq!(kept.day, 14);
    }
}
