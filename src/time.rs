//! Local time without extra crates: Windows does the time zone and DST rules.

use std::ptr::null;
use std::time::{SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows_sys::Win32::System::Time::{
    FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime,
    TzSpecificLocalTimeToSystemTime,
};

pub const HOUR: u64 = 3600;
pub const DAY: u64 = 86_400;
pub const WEEK: u64 = 7 * DAY;
const EPOCH_1601: u64 = 11_644_473_600;
const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn ft(secs: u64) -> FILETIME {
    let v = (secs + EPOCH_1601) * 10_000_000;
    FILETIME { dwLowDateTime: v as u32, dwHighDateTime: (v >> 32) as u32 }
}

fn secs(f: &FILETIME) -> u64 {
    let v = ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
    (v / 10_000_000).saturating_sub(EPOCH_1601)
}

/// Unix seconds -> local wall clock.
pub fn local(s: u64) -> SYSTEMTIME {
    unsafe {
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        FileTimeToSystemTime(&ft(s), &mut utc);
        let mut loc: SYSTEMTIME = std::mem::zeroed();
        SystemTimeToTzSpecificLocalTime(null(), &utc, &mut loc);
        loc
    }
}

/// Local wall clock -> Unix seconds.
fn from_local(st: &SYSTEMTIME) -> u64 {
    unsafe {
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        TzSpecificLocalTimeToSystemTime(null(), st, &mut utc);
        let mut f: FILETIME = std::mem::zeroed();
        SystemTimeToFileTime(&utc, &mut f);
        secs(&f)
    }
}

/// Move a local wall-clock date by whole days (keeps the clock time across DST changes).
fn add_days(st: &SYSTEMTIME, days: u64) -> SYSTEMTIME {
    unsafe {
        let mut f: FILETIME = std::mem::zeroed();
        SystemTimeToFileTime(st, &mut f);
        let mut out: SYSTEMTIME = std::mem::zeroed();
        FileTimeToSystemTime(&ft(secs(&f) + days * DAY), &mut out);
        out
    }
}

fn same_day(a: &SYSTEMTIME, b: &SYSTEMTIME) -> bool {
    a.wYear == b.wYear && a.wMonth == b.wMonth && a.wDay == b.wDay
}

fn clock(st: &SYSTEMTIME, minutes: bool) -> String {
    let h = match st.wHour % 12 {
        0 => 12,
        h => h,
    };
    let ap = if st.wHour < 12 { "AM" } else { "PM" };
    if minutes { format!("{h}:{:02} {ap}", st.wMinute) } else { format!("{h} {ap}") }
}

fn day_name(st: &SYSTEMTIME) -> String {
    let today = local(now());
    if same_day(st, &today) {
        "today".into()
    } else if same_day(st, &local(now() + DAY)) {
        "tomorrow".into()
    } else {
        let month = MONTHS[(st.wMonth as usize).clamp(1, 12) - 1];
        format!("{} {month} {}", DAYS[st.wDayOfWeek as usize % 7], st.wDay)
    }
}

/// When something happens, as precisely as we know it:
/// "Tue Oct 20, 3:12 AM" (within 30 min), "Tue Oct 20, ~3 AM" (within a few hours),
/// "~Tue Oct 20" (roughly). Today and tomorrow are named as such.
pub fn when(s: u64, uncertainty: u64) -> String {
    if uncertainty <= 30 * 60 {
        let st = local(s);
        format!("{}, {}", day_name(&st), clock(&st, true))
    } else if uncertainty <= 5 * HOUR {
        let st = local((s + 1800) / 3600 * 3600);
        format!("{}, ~{}", day_name(&st), clock(&st, false))
    } else {
        format!("~{}", day_name(&local(s)))
    }
}

/// Just the clock, e.g. "3:40 PM".
pub fn clock_at(s: u64) -> String {
    clock(&local(s), true)
}

/// "5d 11h", "3h 20m", "12m".
pub fn span(secs: u64) -> String {
    let (d, h, m) = (secs / DAY, secs % DAY / HOUR, secs % HOUR / 60);
    if d > 0 {
        format!("{d}d {h}h")
    } else if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{}m", m.max(1))
    }
}

/// "9:00 AM", "9am", "21:30" -> (hour, minute)
pub fn parse_clock(text: &str) -> Option<(u16, u16)> {
    let t = text.to_ascii_lowercase();
    let time = t.split_whitespace().find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
    let digits: String = time.chars().take_while(|c| c.is_ascii_digit() || *c == ':').collect();
    let mut parts = digits.split(':');
    let mut h: u16 = parts.next()?.parse().ok()?;
    let m: u16 = match parts.next() {
        Some(x) => x.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None; // "2:30:59"
    }
    if t.contains("pm") && h < 12 {
        h += 12;
    }
    if t.contains("am") && h == 12 {
        h = 0;
    }
    (h <= 23 && m <= 59).then_some((h, m))
}

/// Next time a daily moment like "9:00 AM" comes round.
pub fn next_daily(text: &str) -> Option<u64> {
    let (h, m) = parse_clock(text)?;
    let mut st = local(now());
    st.wHour = h;
    st.wMinute = m;
    st.wSecond = 0;
    st.wMilliseconds = 0;
    let mut at = from_local(&st);
    if at <= now() {
        at = from_local(&add_days(&st, 1));
    }
    Some(at)
}

/// "5m ago", "3h ago", "Tue Oct 13"
pub fn ago(s: u64) -> String {
    let d = now().saturating_sub(s);
    if d < HOUR {
        format!("{}m ago", (d / 60).max(1))
    } else if d < DAY {
        format!("{}h ago", d / HOUR)
    } else {
        let st = local(s);
        format!("{} {} {}", DAYS[st.wDayOfWeek as usize % 7], MONTHS[(st.wMonth as usize).clamp(1, 12) - 1], st.wDay)
    }
}

/// Next time a weekly moment like "Tue 2:00 AM" (or "tue 14:00", "Tuesday 2am") comes round.
pub fn next_weekly(text: &str) -> Option<u64> {
    let t = text.to_ascii_lowercase().replace(['.', ','], " ");
    let words: Vec<&str> = t.split_whitespace().collect();
    let day = words.iter().find_map(|w| w.get(..3).and_then(|w3| DAYS.iter().position(|d| d.to_ascii_lowercase().starts_with(w3))))?;
    let time = words.iter().find(|w| w.starts_with(|c: char| c.is_ascii_digit()))?;
    let pm = t.contains("pm");
    let am = t.contains("am");
    let digits: String = time.chars().take_while(|c| c.is_ascii_digit() || *c == ':').collect();
    let mut parts = digits.split(':');
    let mut h: u16 = parts.next()?.parse().ok()?;
    let m: u16 = match parts.next() {
        Some(x) => x.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None; // "2:30:59"
    }
    if pm && h < 12 {
        h += 12;
    }
    if am && h == 12 {
        h = 0;
    }
    if h > 23 || m > 59 {
        return None;
    }
    let mut st = local(now());
    let ahead = (day as u64 + 7 - st.wDayOfWeek as u64) % 7;
    st.wHour = h;
    st.wMinute = m;
    st.wSecond = 0;
    st.wMilliseconds = 0;
    let mut at = from_local(&add_days(&st, ahead));
    if at <= now() {
        at = from_local(&add_days(&st, ahead + 7));
    }
    Some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weekly_text_parses_to_the_next_week() {
        for s in ["Tue 2:00 AM", "tuesday 2am", "Tue 14:30", "Sun 12:00 PM"] {
            let at = next_weekly(s).unwrap_or_else(|| panic!("{s}"));
            assert!(at > now() && at <= now() + WEEK, "{s}");
        }
        let st = local(next_weekly("Tue 2:00 AM").unwrap());
        assert_eq!((st.wDayOfWeek, st.wHour, st.wMinute), (2, 2, 0));
        assert!(next_weekly("someday").is_none());
        assert!(next_weekly("Mié 2am").is_none()); // not a char boundary at byte 3: no panic
        assert!(next_weekly("Tue 25:00").is_none());
        for bad in ["Tue 2:99999", "Tue 2:garbage", "Tue 2:30:59"] {
            assert!(next_weekly(bad).is_none(), "{bad}");
        }
        assert!(parse_clock("9:99999").is_none() && parse_clock("9:30:59").is_none());
    }

    #[test]
    fn daily_text_parses() {
        let at = next_daily("9:00 AM").unwrap();
        assert!(at > now() && at <= now() + DAY);
        assert_eq!((local(at).wHour, local(at).wMinute), (9, 0));
        assert!(next_daily("21:30").is_some());
        assert!(next_daily("noon").is_none());
    }

    #[test]
    fn spans_read_well() {
        assert_eq!(span(5 * DAY + 11 * HOUR + 3), "5d 11h");
        assert_eq!(span(3 * HOUR + 20 * 60), "3h 20m");
        assert_eq!(span(30), "1m");
    }
}
