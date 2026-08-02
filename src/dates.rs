//! Civil-date arithmetic, hand-rolled rather than pulled from `chrono`.
//!
//! Timestamps are stored as fixed-width ISO-8601 strings and compared lexically
//! in SQL, which is the only reason the range filters work. Display is day-first
//! (`28/04/2026`) and lives in `render::fmt_date`; turning what someone typed
//! into a bound lives here.
//!
//! The upper bound of a range is **exclusive** — the day after the period named —
//! because stored dates carry a time, so an inclusive `<=` on a date-only string
//! would drop the whole of that final day.

use anyhow::{bail, Context, Result};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

pub(crate) fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 10 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse().ok() };
    let (y, m, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let mut secs = days_from_civil(y, m, d) * 86400;
    if b.len() >= 19 {
        secs += num(11, 13)? * 3600 + num(14, 16)? * 60 + num(17, 19)?;
    }
    Some(secs)
}

pub(crate) fn format_iso(epoch: i64) -> String {
    let days = epoch.div_euclid(86400);
    let rem = epoch.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub(crate) fn human_age(secs: i64) -> String {
    if secs < 90 {
        "just now".to_string()
    } else if secs < 5400 {
        format!("{}m old", secs / 60)
    } else if secs < 172800 {
        format!("{}h old", secs / 3600)
    } else {
        format!("{}d old", secs / 86400)
    }
}

/// Accepts `YYYY-MM-DD` or a relative window like `30d` / `6m` / `2y`.
/// The date window a set of flags asks for. `--date` is the documented spelling;
/// `--since` predates it and means the same thing, so whichever is present wins.
pub(crate) fn date_window(
    date: &Option<String>,
    since: &Option<String>,
) -> Result<(Option<String>, Option<String>)> {
    if let Some(v) = date.as_deref() {
        return parse_range(v);
    }
    if let Some(v) = since.as_deref() {
        // The names mean different things and both should keep their meaning:
        // `--date 2024` is *that year*, while `--since 2024` has always been
        // "2024 onwards". Routing the alias through `parse_range` would have
        // silently turned `--since 2026-07-01` into a one-day window.
        if v.contains("..") {
            return parse_range(v);
        }
        return Ok((Some(parse_bound(v, false)?), None));
    }
    Ok((None, None))
}

/// One end of a `--date` range, expanded by how coarse it is.
///
/// With `upper`, the value returned is **exclusive**: the first day *after* the
/// period named, so `..2024` becomes `2025-01-01`. That is not a stylistic choice —
/// `papers.date` holds a full timestamp (`2026-04-28T02:25:24Z`), so an inclusive
/// `date <= "2026-04-28"` excludes everything that actually happened that day. A
/// `<` against the following midnight is both correct and still an index range
/// scan.
///
/// Accepts `28/04/2024`, `04/2024` and `2024` — the same day/month/year shape the
/// tool prints — plus ISO `2024-04-28`, which stays parseable but undocumented so
/// older scripts and habits keep working.
pub(crate) fn parse_bound(s: &str, upper: bool) -> Result<String> {
    let t = s.trim();
    if t.is_empty() {
        bail!("empty date");
    }
    let last_day = |y: i64, m: i64| -> i64 {
        // First of the following month, minus a day: no month-length table, and
        // leap years fall out of the civil-date maths for free.
        let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
        let (_, _, d) = civil_from_days(days_from_civil(ny, nm, 1) - 1);
        d
    };
    let build = |y: i64, m: i64, d: i64| -> Result<String> {
        if !(1..=12).contains(&m) {
            bail!("{m} is not a month — dates are day/month/year, as displayed");
        }
        if !(1..=last_day(y, m)).contains(&d) {
            bail!("{d} is not a day in {m:02}/{y}");
        }
        let (y, m, d) = if upper {
            // One day on, so the comparison can be `<` and still include every
            // timestamp within the named period.
            civil_from_days(days_from_civil(y, m, d) + 1)
        } else {
            (y, m, d)
        };
        Ok(format!("{y:04}-{m:02}-{d:02}"))
    };

    // ISO, kept for compatibility. Every component must parse: defaulting them
    // turned a typo like "2024-o6-01" into 2024-01-01 and answered a question
    // nobody asked, which is worse than refusing.
    if t.len() >= 8 && t.contains('-') && !t.contains('/') {
        let p: Vec<&str> = t.splitn(3, '-').collect();
        let part = |v: Option<&&str>, what: &str| -> Result<i64> {
            match v {
                Some(v) => v.trim().parse().map_err(|_| {
                    anyhow::anyhow!("could not read the {what} in {t:?} — try 28/04/2024 or 2024")
                }),
                None => Ok(1),
            }
        };
        let y = part(p.first(), "year")?;
        let m = part(p.get(1), "month")?;
        let d = part(p.get(2), "day")?;
        return build(y, m, d);
    }

    let parts: Vec<&str> = t.split('/').collect();
    let num = |v: &str| -> Result<i64> {
        v.trim()
            .parse()
            .with_context(|| format!("could not read date {t:?}"))
    };
    match parts.as_slice() {
        // A bare number is a year, unless it carries a relative unit.
        [one] => {
            let one = one.trim();
            if one.len() == 4 && one.chars().all(|c| c.is_ascii_digit()) {
                let y = num(one)?;
                return if upper { build(y, 12, 31) } else { build(y, 1, 1) };
            }
            // 30d, 2y, 1w, 1m — a window ending now.
            let (n, unit) = one.split_at(one.len().saturating_sub(1));
            let n: i64 = n
                .parse()
                .with_context(|| format!("could not read date {t:?}"))?;
            let days = match unit {
                "d" => n,
                "w" => n * 7,
                "m" => n * 30,
                "y" => n * 365,
                _ => bail!("{t:?} is not a date — try 28/04/2024, 04/2024, 2024 or 30d"),
            };
            Ok(format_iso(now() - days * 86400)
                .chars()
                .take(10)
                .collect())
        }
        [m, y] => {
            let (m, y) = (num(m)?, num(y)?);
            if upper {
                build(y, m, last_day(y, m))
            } else {
                build(y, m, 1)
            }
        }
        [d, m, y] => build(num(y)?, num(m)?, num(d)?),
        _ => bail!("{t:?} is not a date — try 28/04/2024, 04/2024, 2024 or 30d"),
    }
}

/// `--date` in full: one flag carrying both ends. `2023..2024`, `2023..`, `..2020`,
/// or a single bound standing for the whole period it names.
pub(crate) fn parse_range(s: &str) -> Result<(Option<String>, Option<String>)> {
    let t = s.trim();
    if let Some((lo, hi)) = t.split_once("..") {
        let (lo, hi) = (lo.trim(), hi.trim());
        if lo.is_empty() && hi.is_empty() {
            bail!("{t:?} has no dates in it");
        }
        let from = if lo.is_empty() {
            None
        } else {
            Some(parse_bound(lo, false)?)
        };
        let till = if hi.is_empty() {
            None
        } else {
            Some(parse_bound(hi, true)?)
        };
        if let (Some(f), Some(t2)) = (&from, &till) {
            if f >= t2 {
                bail!("{t:?} runs backwards — the earlier date goes first");
            }
        }
        return Ok((from, till));
    }
    // No `..`, so the value names a single period and both ends come from it: a
    // year means all of that year, a day means just that day. Only a relative
    // window is open-ended, and that has to be detected positively — treating
    // "anything not a slash date" as relative silently dropped the upper bound
    // from ISO input.
    let from = parse_bound(t, false)?;
    if is_relative(t) {
        return Ok((Some(from), None));
    }
    Ok((Some(from), Some(parse_bound(t, true)?)))
}

/// `30d`, `2y`, `1w`, `1m`: a number and a unit, meaning "since then, until now".
pub(crate) fn is_relative(t: &str) -> bool {
    let (n, unit) = t.split_at(t.len().saturating_sub(1));
    matches!(unit, "d" | "w" | "m" | "y") && !n.is_empty() && n.parse::<i64>().is_ok()
}
