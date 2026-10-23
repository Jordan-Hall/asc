//! Date rendering with git's `--date=` modes (`date.c`), byte for byte.

const WDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const WDAYS_LONG: [&str; 7] =
    ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const MONTHS_LONG: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];

/// Which rendering `--date=<mode>` selects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Default,
    Relative,
    Short,
    Iso,
    IsoStrict,
    Rfc2822,
    Raw,
    Unix,
    Human,
    /// `format:<strftime>`.
    Strftime(String),
}

/// A date mode: the rendering and whether times are shown in the local zone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mode {
    pub kind: Kind,
    pub local: bool,
}

impl Default for Mode {
    fn default() -> Self {
        Self { kind: Kind::Default, local: false }
    }
}

impl Mode {
    /// Parses the value of `--date=`; `None` for a mode git does not know.
    pub fn parse(text: &str) -> Option<Self> {
        if let Some(f) = text.strip_prefix("format:") {
            return Some(Self { kind: Kind::Strftime(f.to_owned()), local: false });
        }
        if let Some(f) = text.strip_prefix("format-local:") {
            return Some(Self { kind: Kind::Strftime(f.to_owned()), local: true });
        }
        let (name, local) = match text.strip_suffix("-local") {
            Some(n) => (n, true),
            None => (text, text == "local"),
        };
        let kind = match name {
            "default" | "local" => Kind::Default,
            "relative" => Kind::Relative,
            "short" => Kind::Short,
            "iso" | "iso8601" => Kind::Iso,
            "iso-strict" | "iso8601-strict" => Kind::IsoStrict,
            "rfc" | "rfc2822" => Kind::Rfc2822,
            "raw" => Kind::Raw,
            "unix" => Kind::Unix,
            "human" => Kind::Human,
            _ => return None,
        };
        Some(Self { kind, local })
    }

    /// `time` (unix seconds) in the zone `tz_min` (minutes east of UTC), as git prints it. `now`
    /// is the reference for relative and human styles.
    pub fn render(&self, time: i64, tz_min: i16, now: i64) -> String {
        let tz = if self.local { local_offset_min() } else { i32::from(tz_min) };
        let c = Civil::at(time, tz);
        match &self.kind {
            Kind::Default if self.local => c.default_form().rsplit_once(' ').map_or_else(String::new, |(a, _)| a.to_owned()),
            Kind::Default => c.default_form(),
            Kind::Relative => relative(time, now),
            Kind::Short => format!("{:04}-{:02}-{:02}", c.year, c.month, c.day),
            Kind::Iso => format!("{} {}", c.iso_date_time(), c.zone()),
            Kind::IsoStrict => c.iso_strict(),
            Kind::Rfc2822 => format!(
                "{}, {} {} {} {:02}:{:02}:{:02} {}",
                WDAYS[c.wday], c.day, MONTHS[c.month as usize - 1], c.year, c.hour, c.min,
                c.sec, c.zone()
            ),
            Kind::Raw => format!("{time} {}", Civil::zone_of(tz)),
            Kind::Unix => time.to_string(),
            Kind::Human => human(time, tz, now),
            Kind::Strftime(f) => strftime(f, &c, time),
        }
    }
}

/// The zone offset of the process, in minutes east of UTC: from `TZ` when it is `UTC`/`GMT` with
/// an optional fixed offset, else UTC (the standard library has no zone database).
fn local_offset_min() -> i32 {
    let Ok(tz) = std::env::var("TZ") else { return 0 };
    let rest = tz.trim_start_matches(|c: char| c.is_ascii_alphabetic());
    let (sign, digits) = match rest.as_bytes().first() {
        Some(b'-') => (1, &rest[1..]), // POSIX offsets point west: `UTC-2` is +02:00.
        Some(b'+') => (-1, &rest[1..]),
        _ => (-1, rest),
    };
    let mut parts = digits.split(':');
    let h: i32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let m: i32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    sign * (h * 60 + m)
}

/// A broken-down time.
struct Civil {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    min: u32,
    sec: u32,
    wday: usize,
    yday: u32,
    tz: i32,
}

impl Civil {
    fn at(time: i64, tz_min: i32) -> Self {
        let local = time + i64::from(tz_min) * 60;
        let days = local.div_euclid(86_400);
        let rem = local.rem_euclid(86_400);
        let (year, month, day) = civil_from_days(days);
        let jan1 = days_from_civil(year, 1, 1);
        Self {
            year,
            month,
            day,
            hour: (rem / 3600) as u32,
            min: (rem % 3600 / 60) as u32,
            sec: (rem % 60) as u32,
            wday: (days + 4).rem_euclid(7) as usize,
            yday: (days - jan1) as u32,
            tz: tz_min,
        }
    }

    /// `+hhmm` / `-hhmm`.
    fn zone_of(tz_min: i32) -> String {
        let sign = if tz_min < 0 { '-' } else { '+' };
        let a = tz_min.abs();
        format!("{sign}{:02}{:02}", a / 60, a % 60)
    }

    fn zone(&self) -> String {
        Self::zone_of(self.tz)
    }

    fn iso_date_time(&self) -> String {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.min, self.sec
        )
    }

    fn iso_strict(&self) -> String {
        let zone = if self.tz == 0 {
            "Z".to_owned()
        } else {
            let a = self.tz.abs();
            format!("{}{:02}:{:02}", if self.tz < 0 { '-' } else { '+' }, a / 60, a % 60)
        };
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{zone}",
            self.year, self.month, self.day, self.hour, self.min, self.sec
        )
    }

    fn default_form(&self) -> String {
        format!(
            "{} {} {} {:02}:{:02}:{:02} {} {}",
            WDAYS[self.wday], MONTHS[self.month as usize - 1], self.day, self.hour, self.min,
            self.sec, self.year, self.zone()
        )
    }
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The inverse of [`civil_from_days`].
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `show_date_relative`.
fn relative(time: i64, now: i64) -> String {
    if time > now {
        return "in the future".into();
    }
    let mut diff = now - time;
    if diff < 90 {
        return format!("{} ago", plural(diff, "second", "seconds"));
    }
    diff = (diff + 30) / 60;
    if diff < 90 {
        return format!("{} ago", plural(diff, "minute", "minutes"));
    }
    diff = (diff + 30) / 60;
    if diff < 36 {
        return format!("{} ago", plural(diff, "hour", "hours"));
    }
    diff = (diff + 12) / 24;
    if diff < 14 {
        return format!("{} ago", plural(diff, "day", "days"));
    }
    if diff < 70 {
        return format!("{} ago", plural((diff + 3) / 7, "week", "weeks"));
    }
    if diff < 365 {
        return format!("{} ago", plural((diff + 15) / 30, "month", "months"));
    }
    if diff < 1825 {
        let total = (diff * 12 * 2 + 365) / (365 * 2);
        let (years, months) = (total / 12, total % 12);
        let y = plural(years, "year", "years");
        return if months > 0 {
            format!("{y}, {} ago", plural(months, "month", "months"))
        } else {
            format!("{y} ago")
        };
    }
    format!("{} ago", plural((diff + 183) / 365, "year", "years"))
}

/// `--date=human`: relative today, a weekday within the last days of this month, the month and
/// day within this year, else the full date.
fn human(time: i64, tz: i32, now: i64) -> String {
    let (t, n) = (Civil::at(time, tz), Civil::at(now, tz));
    let month = MONTHS[t.month as usize - 1];
    if t.year != n.year {
        return format!("{month} {} {}", t.day, t.year);
    }
    if t.month == n.month && t.day == n.day {
        return relative(time, now);
    }
    if t.month == n.month && t.day < n.day && n.day - t.day < 5 {
        return format!("{} {:02}:{:02}", WDAYS[t.wday], t.hour, t.min);
    }
    format!("{} {month} {} {:02}:{:02}", WDAYS[t.wday], t.day, t.hour, t.min)
}

/// The subset of `strftime` git users reach for in `--date=format:`.
fn strftime(f: &str, c: &Civil, time: i64) -> String {
    let mut out = String::new();
    let mut it = f.chars();
    while let Some(ch) = it.next() {
        if ch != '%' {
            out.push(ch);
            continue;
        }
        let Some(spec) = it.next() else {
            out.push('%');
            break;
        };
        match spec {
            'Y' => out.push_str(&c.year.to_string()),
            'C' => out.push_str(&format!("{:02}", c.year.div_euclid(100))),
            'y' => out.push_str(&format!("{:02}", c.year.rem_euclid(100))),
            'm' => out.push_str(&format!("{:02}", c.month)),
            'd' => out.push_str(&format!("{:02}", c.day)),
            'e' => out.push_str(&format!("{:2}", c.day)),
            'H' => out.push_str(&format!("{:02}", c.hour)),
            'I' => out.push_str(&format!("{:02}", (c.hour + 11) % 12 + 1)),
            'M' => out.push_str(&format!("{:02}", c.min)),
            'S' => out.push_str(&format!("{:02}", c.sec)),
            'p' => out.push_str(if c.hour < 12 { "AM" } else { "PM" }),
            'a' => out.push_str(WDAYS[c.wday]),
            'A' => out.push_str(WDAYS_LONG[c.wday]),
            'b' | 'h' => out.push_str(MONTHS[c.month as usize - 1]),
            'B' => out.push_str(MONTHS_LONG[c.month as usize - 1]),
            'j' => out.push_str(&format!("{:03}", c.yday + 1)),
            'u' => out.push_str(&(if c.wday == 0 { 7 } else { c.wday }).to_string()),
            'w' => out.push_str(&c.wday.to_string()),
            'z' => out.push_str(&c.zone()),
            'Z' => {}
            's' => out.push_str(&time.to_string()),
            'F' => out.push_str(&format!("{:04}-{:02}-{:02}", c.year, c.month, c.day)),
            'T' => out.push_str(&format!("{:02}:{:02}:{:02}", c.hour, c.min, c.sec)),
            'R' => out.push_str(&format!("{:02}:{:02}", c.hour, c.min)),
            'D' => out.push_str(&format!("{:02}/{:02}/{:02}", c.month, c.day, c.year.rem_euclid(100))),
            'c' => out.push_str(&format!(
                "{} {} {:2} {:02}:{:02}:{:02} {}",
                WDAYS[c.wday], MONTHS[c.month as usize - 1], c.day, c.hour, c.min, c.sec, c.year
            )),
            'n' => out.push('\n'),
            't' => out.push('\t'),
            '%' => out.push('%'),
            other => {
                out.push('%');
                out.push(other);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(mode: &str, time: i64, tz: i16, now: i64) -> String {
        Mode::parse(mode).unwrap().render(time, tz, now)
    }

    #[test]
    fn fixed_forms() {
        let t = 1_700_000_000;
        assert_eq!(render("default", t, 0, t), "Tue Nov 14 22:13:20 2023 +0000");
        assert_eq!(render("iso", t, 90, t), "2023-11-14 23:43:20 +0130");
        assert_eq!(render("iso-strict", t, 0, t), "2023-11-14T22:13:20Z");
        assert_eq!(render("iso-strict", t, -300, t), "2023-11-14T17:13:20-05:00");
        assert_eq!(render("rfc", t, 0, t), "Tue, 14 Nov 2023 22:13:20 +0000");
        assert_eq!(render("short", t, 0, t), "2023-11-14");
        assert_eq!(render("raw", t, -330, t), "1700000000 -0530");
        assert_eq!(render("unix", t, 0, t), "1700000000");
        assert_eq!(render("format:%Y/%m/%d %H:%M", t, 0, t), "2023/11/14 22:13");
    }

    #[test]
    fn relative_ladder() {
        let now = 1_000_000_000;
        assert_eq!(render("relative", now - 1, 0, now), "1 second ago");
        assert_eq!(render("relative", now - 3 * 3600, 0, now), "3 hours ago");
        assert_eq!(render("relative", now - 3 * 86_400, 0, now), "3 days ago");
        assert_eq!(render("relative", now - 400 * 86_400, 0, now), "1 year, 1 month ago");
        assert_eq!(render("relative", now + 5, 0, now), "in the future");
    }

    #[test]
    fn unknown_mode_is_none() {
        assert!(Mode::parse("bogus").is_none());
        assert!(Mode::parse("iso-local").unwrap().local);
    }
}
