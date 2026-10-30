//! Bounded civil calendar calculations. A nonexistent local time moves to the
//! first valid instant after the gap; a repeated local time uses its first occurrence.
use excalibur_store::{Error, Result};
use jiff::{
    Timestamp,
    civil::{Date, DateTime},
    tz::TimeZone,
};

fn invalid() -> Error {
    Error::Invalid("invalid calendar expression, IANA zone or timestamp")
}
pub fn validate_timezone(name: &str) -> Result<()> {
    zone(name).map(|_| ())
}
fn zone(name: &str) -> Result<TimeZone> {
    if name.is_empty() || name.len() > 256 {
        return Err(invalid());
    }
    let zone = TimeZone::get(name).map_err(|_| invalid())?;
    if name != "UTC" && zone.iana_name().is_none() {
        return Err(invalid());
    }
    Ok(zone)
}
fn timestamp(value: i64) -> Result<Timestamp> {
    Timestamp::from_second(value).map_err(|_| invalid())
}

/// Whole-second local date/time, parsed independently from its IANA time zone.
/// Unlike RFC5545 compatible shifting, a gap maps to its end, not the same minute
/// after the gap (02:30 -> 03:00, rather than 03:30 for a one-hour jump).
pub fn local_to_utc(local: &str, timezone: &str) -> Result<i64> {
    if local.len() > 64 {
        return Err(invalid());
    }
    let separator = local.find(['T', 't', ' ']).ok_or_else(invalid)?;
    if local[separator + 1..].contains(['+', '-', 'Z', 'z', '[', ']']) {
        return Err(invalid());
    }
    let local: DateTime = local.parse().map_err(|_| invalid())?;
    if local.subsec_nanosecond() != 0 {
        return Err(invalid());
    }
    resolve(local, &zone(timezone)?)
}
fn resolve(local: DateTime, zone: &TimeZone) -> Result<i64> {
    let compatible = zone
        .to_ambiguous_zoned(local)
        .compatible()
        .map_err(|_| invalid())?;
    if compatible.datetime() == local {
        return Ok(compatible.timestamp().as_second());
    }
    let upper = timestamp(
        compatible
            .timestamp()
            .as_second()
            .checked_add(1)
            .ok_or_else(invalid)?,
    )?;
    let transition = zone.preceding(upper).next().ok_or_else(invalid)?;
    let next = transition.timestamp();
    let previous = timestamp(next.as_second().checked_sub(1).ok_or_else(invalid)?)?;
    if zone.to_datetime(previous) >= local || zone.to_datetime(next) <= local {
        return Err(invalid());
    }
    Ok(next.as_second())
}
#[derive(Clone, Debug)]
struct Field {
    values: Vec<u8>,
    wildcard: bool,
}
impl Field {
    fn parse(input: &str, min: u8, max: u8, sunday: bool) -> Result<Self> {
        let mut present = [false; 64];
        if input.is_empty() || input.len() > 256 {
            return Err(invalid());
        }
        for part in input.split(',') {
            let (base, step) = match part.split_once('/') {
                Some((base, n)) => (base, n.parse::<u8>().map_err(|_| invalid())?),
                None => (part, 1),
            };
            if step == 0 {
                return Err(invalid());
            }
            let (start, end) = if base == "*" {
                (min, max)
            } else if let Some((a, b)) = base.split_once('-') {
                (
                    a.parse::<u8>().map_err(|_| invalid())?,
                    b.parse::<u8>().map_err(|_| invalid())?,
                )
            } else {
                let n = base.parse::<u8>().map_err(|_| invalid())?;
                (n, if part.contains('/') { max } else { n })
            };
            if start < min || end > max || start > end {
                return Err(invalid());
            }
            for n in (start..=end).step_by(usize::from(step)) {
                present[usize::from(if sunday && n == 7 { 0 } else { n })] = true;
            }
        }
        let values = (0..64u8)
            .filter(|n| present[usize::from(*n)])
            .collect::<Vec<_>>();
        if values.is_empty() {
            return Err(invalid());
        }
        Ok(Self {
            values,
            wildcard: input.starts_with('*'),
        })
    }
    fn has(&self, n: u8) -> bool {
        self.values.contains(&n)
    }
}
/// Numeric five fields (minute hour day month weekday) or six fields with
/// leading seconds. Lists, inclusive ranges and positive steps are supported.
/// Restricted day-of-month and weekday use the standard OR combination.
#[derive(Clone, Debug)]
pub struct Cron {
    seconds: Field,
    minutes: Field,
    hours: Field,
    days: Field,
    months: Field,
    weekdays: Field,
}
impl Cron {
    pub fn parse(expression: &str) -> Result<Self> {
        if expression.len() > 1024 {
            return Err(invalid());
        }
        let mut fields = expression.split_whitespace().collect::<Vec<_>>();
        if fields.len() == 5 {
            fields.insert(0, "0");
        }
        if fields.len() != 6 {
            return Err(invalid());
        }
        Ok(Self {
            seconds: Field::parse(fields[0], 0, 59, false)?,
            minutes: Field::parse(fields[1], 0, 59, false)?,
            hours: Field::parse(fields[2], 0, 23, false)?,
            days: Field::parse(fields[3], 1, 31, false)?,
            months: Field::parse(fields[4], 1, 12, false)?,
            weekdays: Field::parse(fields[5], 0, 7, true)?,
        })
    }
    fn matches_date(&self, date: Date) -> bool {
        if !self.months.has(date.month() as u8) {
            return false;
        }
        let dom = self.days.has(date.day() as u8);
        let dow = self
            .weekdays
            .has(date.weekday().to_sunday_zero_offset() as u8);
        if self.days.wildcard || self.weekdays.wildcard {
            dom && dow
        } else {
            dom || dow
        }
    }
    /// Strictly after the supplied UTC second. Search is bounded to eight years;
    /// impossible expressions fail instead of monopolizing a daemon thread.
    pub fn next_after(&self, after: i64, timezone: &str) -> Result<i64> {
        self.search(after, timezone, false)
    }
    /// Latest occurrence at or before the supplied UTC second.
    pub fn latest_at(&self, before: i64, timezone: &str) -> Result<i64> {
        self.search(before, timezone, true)
    }
    fn search(&self, bound: i64, timezone: &str, reverse: bool) -> Result<i64> {
        let zone = zone(timezone)?;
        let local = zone.to_datetime(timestamp(bound)?);
        let mut date = local.date();
        for _ in 0..=366 * 8 {
            if self.matches_date(date) {
                let hours: Box<dyn Iterator<Item = &u8>> = if reverse {
                    Box::new(self.hours.values.iter().rev())
                } else {
                    Box::new(self.hours.values.iter())
                };
                for hour in hours {
                    let minutes: Box<dyn Iterator<Item = &u8>> = if reverse {
                        Box::new(self.minutes.values.iter().rev())
                    } else {
                        Box::new(self.minutes.values.iter())
                    };
                    for minute in minutes {
                        let seconds: Box<dyn Iterator<Item = &u8>> = if reverse {
                            Box::new(self.seconds.values.iter().rev())
                        } else {
                            Box::new(self.seconds.values.iter())
                        };
                        for second in seconds {
                            let candidate = date.at(*hour as i8, *minute as i8, *second as i8, 0);
                            // In a fold reverse search must still consider a later
                            // civil minute in its FIRST occurrence before bound.
                            if !reverse && candidate <= local {
                                continue;
                            }
                            let value = resolve(candidate, &zone)?;
                            if (reverse && value <= bound) || (!reverse && value > bound) {
                                return Ok(value);
                            }
                        }
                    }
                }
            }
            date = if reverse {
                date.yesterday()
            } else {
                date.tomorrow()
            }
            .map_err(|_| invalid())?;
        }
        Err(Error::Invalid("no cron occurrence within eight years"))
    }
}
