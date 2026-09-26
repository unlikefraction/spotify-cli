//! Positions, offsets and percentages: `90`, `90s`, `1:30`, `1m30s`, `01:02:03`, `25%`, `+15s`, `-10%`.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// A point or span inside a track, either absolute time or a share of the track length.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "unit", content = "value", rename_all = "snake_case")]
pub enum Amount {
    /// Milliseconds.
    Millis(u64),
    /// Percent of the track length, 0–100.
    Percent(f64),
}

impl Amount {
    /// Resolves to milliseconds for a track of `duration_ms`.
    #[must_use]
    pub fn resolve_ms(self, duration_ms: u64) -> u64 {
        match self {
            Self::Millis(ms) => ms,
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss
            )]
            Self::Percent(percent) => ((duration_ms as f64) * percent / 100.0).round() as u64,
        }
    }

    /// Parses `90`, `90s`, `1:30`, `1m30s`, `250ms`, `01:02:03` or `25%`.
    ///
    /// # Errors
    /// Returns `invalid_input` with the accepted forms.
    pub fn parse(input: &str) -> Result<Self> {
        let value = input.trim();
        if let Some(percent) = value.strip_suffix('%') {
            let percent: f64 = percent.trim().parse().map_err(|_| bad(input))?;
            if !(0.0..=100.0).contains(&percent) || !percent.is_finite() {
                return Err(Error::invalid(
                    format!("`{input}` is outside 0%–100%."),
                    "Use a percentage between 0% and 100%, for example 25% or 50%.",
                ));
            }
            return Ok(Self::Percent(percent));
        }
        parse_millis(value)
            .map(Self::Millis)
            .ok_or_else(|| bad(input))
    }
}

impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Millis(ms) => f.write_str(&clock(*ms)),
            Self::Percent(p) => write!(f, "{p}%"),
        }
    }
}

/// Where to seek: an absolute target or an offset from the current position.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeekTarget {
    /// Go to this point.
    Absolute(Amount),
    /// Move by this many milliseconds (negative rewinds).
    RelativeMillis(i64),
    /// Move by this share of the track (negative rewinds).
    RelativePercent(f64),
}

impl SeekTarget {
    /// Parses `1:30`, `50%`, `+15s`, `-10`, `+10%`.
    ///
    /// # Errors
    /// Returns `invalid_input` with the accepted forms.
    pub fn parse(input: &str) -> Result<Self> {
        let value = input.trim();
        let (sign, rest) = match value.as_bytes().first() {
            Some(b'+') => (1_i64, &value[1..]),
            Some(b'-') => (-1_i64, &value[1..]),
            _ => return Amount::parse(value).map(Self::Absolute),
        };
        match Amount::parse(rest)? {
            #[allow(clippy::cast_possible_wrap)]
            Amount::Millis(ms) => Ok(Self::RelativeMillis(sign * ms as i64)),
            #[allow(clippy::cast_precision_loss)]
            Amount::Percent(p) => Ok(Self::RelativePercent(sign as f64 * p)),
        }
    }

    /// Resolves to an absolute position in milliseconds, clamped to the track.
    #[must_use]
    pub fn resolve_ms(self, position_ms: u64, duration_ms: u64) -> u64 {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_possible_wrap,
            clippy::cast_precision_loss,
            clippy::cast_sign_loss
        )]
        let target: i64 = match self {
            Self::Absolute(amount) => amount.resolve_ms(duration_ms) as i64,
            Self::RelativeMillis(delta) => position_ms as i64 + delta,
            Self::RelativePercent(p) => {
                position_ms as i64 + ((duration_ms as f64) * p / 100.0).round() as i64
            }
        };
        #[allow(clippy::cast_sign_loss)]
        let clamped = target.max(0) as u64;
        if duration_ms == 0 {
            clamped
        } else {
            clamped.min(duration_ms.saturating_sub(1))
        }
    }
}

fn bad(input: &str) -> Error {
    Error::invalid(
        format!("`{input}` is not a time or percentage."),
        "Use seconds (90 or 90s), minutes:seconds (1:30), 1m30s, 250ms, hours:minutes:seconds (1:02:03) or a percentage (25%). Seek also accepts +15s / -10s / +10%.",
    )
}

fn parse_millis(value: &str) -> Option<u64> {
    if value.is_empty() {
        return None;
    }
    if value.contains(':') {
        let parts: Vec<&str> = value.split(':').collect();
        if parts.len() > 3 || parts.iter().any(|part| part.is_empty()) {
            return None;
        }
        let mut total = 0.0_f64;
        for (index, part) in parts.iter().enumerate() {
            let number: f64 = part.parse().ok()?;
            if number < 0.0 || !number.is_finite() {
                return None;
            }
            // Only the last component may be fractional; later components must be < 60.
            if index + 1 < parts.len() && number.fract() != 0.0 {
                return None;
            }
            if index > 0 && number >= 60.0 {
                return None;
            }
            total = total * 60.0 + number;
        }
        return to_ms(total);
    }
    if let Some(ms) = value.strip_suffix("ms") {
        return ms.trim().parse::<u64>().ok();
    }
    // 1h2m3s, 1m30s, 90s, 1.5m
    let mut total = 0.0_f64;
    let mut number = String::new();
    let mut saw_unit = false;
    for ch in value.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
            continue;
        }
        let factor = match ch {
            'h' | 'H' => 3600.0,
            'm' | 'M' => 60.0,
            's' | 'S' => 1.0,
            _ => return None,
        };
        let parsed: f64 = number.parse().ok()?;
        total += parsed * factor;
        number.clear();
        saw_unit = true;
    }
    if !number.is_empty() {
        if saw_unit {
            return None;
        }
        total += number.parse::<f64>().ok()?;
    }
    to_ms(total)
}

fn to_ms(seconds: f64) -> Option<u64> {
    if !seconds.is_finite() || seconds < 0.0 || seconds > 1_000_000.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((seconds * 1000.0).round() as u64)
}

/// Formats milliseconds as `m:ss` or `h:mm:ss`.
#[must_use]
pub fn clock(ms: u64) -> String {
    let total = ms / 1000;
    let (hours, minutes, seconds) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// [`clock`] rounded to the nearest second instead of down, for measured spans such as the
/// time left when a trigger fired: 19 782 ms left shows `0:20`, not `0:19`.
#[must_use]
pub fn clock_rounded(ms: u64) -> String {
    clock(ms.saturating_add(500) / 1000 * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_times() {
        assert_eq!(Amount::parse("90").expect("90"), Amount::Millis(90_000));
        assert_eq!(Amount::parse("90s").expect("90s"), Amount::Millis(90_000));
        assert_eq!(Amount::parse("1:30").expect("1:30"), Amount::Millis(90_000));
        assert_eq!(
            Amount::parse("1m30s").expect("1m30s"),
            Amount::Millis(90_000)
        );
        assert_eq!(
            Amount::parse("1:02:03").expect("h"),
            Amount::Millis(3_723_000)
        );
        assert_eq!(Amount::parse("250ms").expect("ms"), Amount::Millis(250));
        assert_eq!(Amount::parse("2.5").expect("frac"), Amount::Millis(2_500));
        assert_eq!(Amount::parse("25%").expect("pct"), Amount::Percent(25.0));
        for bad in ["", "abc", "1:75", "150%", "1:", "5x", "1m30"] {
            assert!(Amount::parse(bad).is_err(), "{bad} must fail");
        }
    }

    #[test]
    fn seeks_resolve_and_clamp() {
        let d = 200_000;
        assert_eq!(
            SeekTarget::parse("+15s").expect("+").resolve_ms(10_000, d),
            25_000
        );
        assert_eq!(
            SeekTarget::parse("-15s").expect("-").resolve_ms(10_000, d),
            0
        );
        assert_eq!(
            SeekTarget::parse("50%").expect("%").resolve_ms(0, d),
            100_000
        );
        assert_eq!(
            SeekTarget::parse("+10%").expect("+%").resolve_ms(0, d),
            20_000
        );
        assert_eq!(
            SeekTarget::parse("9:00").expect("far").resolve_ms(0, d),
            d - 1
        );
    }

    #[test]
    fn clock_formats() {
        assert_eq!(clock(61_000), "1:01");
        assert_eq!(clock(3_723_000), "1:02:03");
        assert_eq!(clock(19_782), "0:19", "positions round down");
        assert_eq!(clock_rounded(19_782), "0:20");
        assert_eq!(clock_rounded(19_499), "0:19");
        assert_eq!(clock_rounded(59_500), "1:00");
        assert_eq!(clock_rounded(0), "0:00");
    }
}
