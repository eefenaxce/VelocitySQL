//! The `interval` value representation.

use std::fmt;

use crate::error::TypeError;
use serde::{Deserialize, Serialize};

/// A PostgreSQL `interval` value.
///
/// PostgreSQL keeps months, days and microseconds in separate fields because
/// they do not convert losslessly into each other (a month may be 28-31 days).
/// We preserve the same triple so that `'1 month'::interval + '2024-01-31'::date`
/// can be evaluated the same way PostgreSQL evaluates it.
///
/// For *comparison* and *sorting* only, 1 month is approximated as 30 days and
/// 1 day as 24 hours, matching PostgreSQL's `interval_cmp` behaviour.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Interval {
    /// Whole months.
    pub months: i32,
    /// Whole days.
    pub days: i32,
    /// Sub-day remainder in microseconds.
    pub micros: i64,
}

/// Microseconds in a 30-day month, PostgreSQL's canonical comparison constant.
const MICROS_PER_MONTH: i64 = 30 * 24 * 60 * 60 * 1_000_000;
/// Microseconds in a day.
const MICROS_PER_DAY: i64 = 24 * 60 * 60 * 1_000_000;

impl Interval {
    /// The zero interval.
    pub const ZERO: Interval = Interval {
        months: 0,
        days: 0,
        micros: 0,
    };

    /// Builds an interval from a number of months.
    pub fn from_months(months: i32) -> Self {
        Interval {
            months,
            ..Interval::ZERO
        }
    }

    /// Builds an interval from a number of days.
    pub fn from_days(days: i32) -> Self {
        Interval {
            days,
            ..Interval::ZERO
        }
    }

    /// Builds an interval from a number of microseconds.
    pub fn from_micros(micros: i64) -> Self {
        Interval {
            micros,
            ..Interval::ZERO
        }
    }

    /// Total time spanned, using PostgreSQL's 30-day month approximation.
    pub fn total_micros(&self) -> i64 {
        self.months as i64 * MICROS_PER_MONTH + self.days as i64 * MICROS_PER_DAY + self.micros
    }

    /// Parses a PostgreSQL interval literal such as `1 year 2 mons 3 days 04:05:06`.
    ///
    /// Supported forms:
    /// - `[N unit]...` where `unit` is one of year(s)/mon(s)/month(s)/week(s)/day(s)/
    ///   hour(s)/minute(s)/min(s)/second(s)/sec(s)/millisecond(s)/microsecond(s)
    /// - ISO-8601-ish `HH:MM[:SS[.ffffff]]`, with an optional leading `-`
    /// - Year-month form `Y-M`
    pub fn parse(input: &str) -> Result<Interval, TypeError> {
        let text = input.trim();
        if text.is_empty() {
            return Err(invalid(input));
        }
        // Bare time-of-day form, e.g. `04:05:06`.
        if !text.contains(' ') && text.contains(':') {
            let (sign, body) = strip_sign(text);
            let micros = parse_time_body(body).ok_or_else(|| invalid(input))?;
            return Ok(Interval::from_micros(sign * micros));
        }
        // Year-month form, e.g. `1-6` meaning 1 year 6 months.
        if !text.contains(' ') && !text.contains(':') {
            if let Some((y, m)) = text.split_once('-') {
                if let (Ok(years), Ok(months)) = (y.trim().parse::<i32>(), m.trim().parse::<i32>())
                {
                    return Ok(Interval::from_months(years * 12 + months));
                }
            }
        }

        let tokens: Vec<&str> = text.split_whitespace().collect();
        let mut out = Interval::ZERO;
        let mut index = 0usize;
        let mut matched = false;
        while index < tokens.len() {
            let token = tokens[index];
            index += 1;
            // `04:05:06` may appear either standalone or after `N unit` pairs.
            if token.contains(':') {
                let (sign, body) = strip_sign(token);
                let micros = parse_time_body(body).ok_or_else(|| invalid(input))?;
                out.micros += sign * micros;
                matched = true;
                continue;
            }
            let (sign, number) = strip_sign(token);
            let value: f64 = number.parse().map_err(|_| invalid(input))?;
            if index >= tokens.len() {
                return Err(invalid(input));
            }
            let unit = tokens[index].trim_end_matches(',').to_ascii_lowercase();
            index += 1;
            matched = true;
            let months_delta: i64 = match unit.as_str() {
                "year" | "years" | "yr" | "yrs" | "y" => (value * 12.0) as i64,
                "mon" | "mons" | "month" | "months" => value as i64,
                _ => 0,
            };
            let days_delta: i64 = match unit.as_str() {
                "week" | "weeks" | "w" => (value * 7.0) as i64,
                "day" | "days" | "d" => value as i64,
                _ => 0,
            };
            let micros_delta: i64 = match unit.as_str() {
                "hour" | "hours" | "hr" | "hrs" | "h" => (value * 3_600_000_000.0) as i64,
                "minute" | "minutes" | "min" | "mins" | "m" => (value * 60_000_000.0) as i64,
                "second" | "seconds" | "sec" | "secs" | "s" => (value * 1_000_000.0) as i64,
                "millisecond" | "milliseconds" | "ms" => (value * 1_000.0) as i64,
                "microsecond" | "microseconds" | "us" => value as i64,
                "year" | "years" | "yr" | "yrs" | "y" | "mon" | "mons" | "month" | "months"
                | "week" | "weeks" | "w" | "day" | "days" | "d" => 0,
                _ => return Err(invalid(input)),
            };
            out.months += (sign * months_delta) as i32;
            out.days += (sign * days_delta) as i32;
            out.micros += sign * micros_delta;
        }
        if !matched {
            return Err(invalid(input));
        }
        Ok(out)
    }
}

fn invalid(input: &str) -> TypeError {
    TypeError::InvalidInputSyntax {
        ty: "interval".into(),
        input: input.to_string(),
    }
}

fn strip_sign(text: &str) -> (i64, &str) {
    if let Some(rest) = text.strip_prefix('-') {
        (-1, rest)
    } else if let Some(rest) = text.strip_prefix('+') {
        (1, rest)
    } else {
        (1, text)
    }
}

/// Parses `HH:MM` or `HH:MM:SS[.ffffff]` into microseconds.
fn parse_time_body(body: &str) -> Option<i64> {
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let hours: i64 = parts[0].trim().parse().ok()?;
    let minutes: i64 = parts[1].trim().parse().ok()?;
    let seconds_micros = if parts.len() == 3 {
        parse_seconds_micros(parts[2])?
    } else {
        0
    };
    Some((hours * 3600 + minutes * 60) * 1_000_000 + seconds_micros)
}

fn parse_seconds_micros(text: &str) -> Option<i64> {
    let (secs, frac) = match text.trim().split_once('.') {
        Some((s, f)) => (s, Some(f)),
        None => (text.trim(), None),
    };
    let seconds: i64 = secs.parse().ok()?;
    let micros = match frac {
        None => 0,
        Some(frac) => {
            let mut digits = frac.to_string();
            digits.truncate(6);
            while digits.len() < 6 {
                digits.push('0');
            }
            digits.parse::<i64>().ok()?
        }
    };
    Some(seconds * 1_000_000 + micros)
}

impl fmt::Display for Interval {
    /// Renders like PostgreSQL's `interval_out`, e.g. `1 year 2 days 03:04:05`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut wrote = false;
        let years = self.months / 12;
        let months = self.months % 12;
        if years != 0 {
            write!(f, "{} year{}", years, plural(years))?;
            wrote = true;
        }
        if months != 0 {
            if wrote {
                f.write_str(" ")?;
            }
            write!(f, "{} mon{}", months, plural(months))?;
            wrote = true;
        }
        if self.days != 0 {
            if wrote {
                f.write_str(" ")?;
            }
            write!(f, "{} day{}", self.days, plural(self.days))?;
            wrote = true;
        }
        if self.micros != 0 || !wrote {
            if wrote {
                f.write_str(" ")?;
            }
            let negative = self.micros < 0;
            let abs = self.micros.unsigned_abs();
            let hours = abs / 3_600_000_000;
            let minutes = (abs / 60_000_000) % 60;
            let seconds = (abs / 1_000_000) % 60;
            let micros = abs % 1_000_000;
            if negative {
                f.write_str("-")?;
            }
            if micros != 0 {
                write!(f, "{hours:02}:{minutes:02}:{seconds:02}.{micros:06}")?;
            } else {
                write!(f, "{hours:02}:{minutes:02}:{seconds:02}")?;
            }
        }
        Ok(())
    }
}

fn plural(n: i32) -> &'static str {
    if n.abs() == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unit_lists() {
        assert_eq!(Interval::parse("1 day").unwrap(), Interval::from_days(1));
        assert_eq!(
            Interval::parse("1 year").unwrap(),
            Interval::from_months(12)
        );
        assert_eq!(Interval::parse("2 mons").unwrap(), Interval::from_months(2));
        assert_eq!(
            Interval::parse("1-6").unwrap(),
            Interval::from_months(18),
            "year-month form"
        );
        assert_eq!(
            Interval::parse("1 year 2 mons 3 days 04:05:06").unwrap(),
            Interval {
                months: 14,
                days: 3,
                micros: 14_706_000_000
            }
        );
    }

    #[test]
    fn rejects_garbage() {
        assert!(Interval::parse("soon").is_err());
        assert!(Interval::parse("").is_err());
        assert!(Interval::parse("1 fortnight").is_err());
    }
}
