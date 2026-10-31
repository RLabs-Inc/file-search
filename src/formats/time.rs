//! Every timestamp flavour we know, in both directions:
//! numbers → readable dates (annotations), and dates → number ranges (search).

use chrono::{DateTime, Local, NaiveDate, TimeZone, Utc};

pub const MAC_EPOCH: i64 = 978_307_200; // 2001-01-01, Cocoa / Core Data / iOS
pub const WEBKIT_EPOCH: i64 = 11_644_473_600; // 1601-01-01, Chrome / WebKit / Windows FILETIME
pub const HFS_EPOCH: i64 = 2_082_844_800; // 1904-01-01, HFS+ / classic Mac / QuickTime

const UNIX_2000: f64 = 946_684_800.0;
const UNIX_2040: f64 = 2_208_988_800.0;

fn fmt_unix(secs: f64, label: &str) -> String {
    let utc: DateTime<Utc> = Utc.timestamp_opt(secs.floor() as i64, 0).single().unwrap_or_default();
    let local = utc.with_timezone(&Local);
    format!(
        "⟨{label} {} UTC · {} local⟩",
        utc.format("%Y-%m-%d %H:%M:%S"),
        local.format("%Y-%m-%d %H:%M:%S")
    )
}

/// Human-readable interpretations of a number that looks like a timestamp.
/// `hinted` = the column/key name suggests a date, which unlocks riskier epochs.
pub fn timestamp_notes(v: f64, hinted: bool) -> Option<String> {
    let mut notes = Vec::new();
    let ok = |s: f64| (UNIX_2000..UNIX_2040).contains(&s);
    let (mac, webkit, hfs) = (MAC_EPOCH as f64, WEBKIT_EPOCH as f64, HFS_EPOCH as f64);
    if ok(v) {
        notes.push(fmt_unix(v, "unix"));
    } else if ok(v / 1e3) {
        notes.push(fmt_unix(v / 1e3, "unix-ms"));
    } else if ok(v / 1e6) && hinted {
        notes.push(fmt_unix(v / 1e6, "unix-µs"));
    }
    if hinted || v.fract() != 0.0 {
        if ok(v + mac) && v > 1e8 {
            notes.push(fmt_unix(v + mac, "mac"));
        } else if ok(v / 1e9 + mac) && v > 1e16 {
            notes.push(fmt_unix(v / 1e9 + mac, "mac-ns"));
        }
    }
    if hinted && ok(v - hfs) {
        notes.push(fmt_unix(v - hfs, "hfs+"));
    }
    if ok(v / 1e6 - webkit) && v > 1e16 {
        notes.push(fmt_unix(v / 1e6 - webkit, "webkit"));
    }
    if ok(v / 1e7 - webkit) && v > 1e17 {
        notes.push(fmt_unix(v / 1e7 - webkit, "filetime"));
    }
    if notes.is_empty() { None } else { Some(notes.join(" ")) }
}

pub fn name_hints_time(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["date", "time", "stamp", "created", "modified", "updated", "visit", "expire", "_at", "last", "when", "start", "end"]
        .iter()
        .any(|k| n.contains(k))
}

/// The unix-second range covering `day` in both local time and UTC
/// (lenient on purpose: a "date" means different instants in different zones).
pub fn day_range(day: NaiveDate) -> (i64, i64) {
    let start = day.and_hms_opt(0, 0, 0).unwrap();
    let end = day.and_hms_opt(23, 59, 59).unwrap();
    let utc = (start.and_utc().timestamp(), end.and_utc().timestamp());
    let local = (
        Local.from_local_datetime(&start).earliest().map(|d| d.timestamp()).unwrap_or(utc.0),
        Local.from_local_datetime(&end).latest().map(|d| d.timestamp()).unwrap_or(utc.1),
    );
    (utc.0.min(local.0), utc.1.max(local.1))
}

/// (epoch label, low, high) integer ranges a timestamp of that day can have.
pub fn epoch_ranges(lo: i64, hi: i64) -> Vec<(&'static str, u64, u64)> {
    let mut v = Vec::new();
    let mut add = |label, a: i128, b: i128| {
        if a > 0 && b < u64::MAX as i128 {
            v.push((label, a as u64, b as u64));
        }
    };
    let (lo, hi) = (lo as i128, hi as i128);
    add("unix", lo, hi);
    add("unix-ms", lo * 1_000, hi * 1_000 + 999);
    add("unix-µs", lo * 1_000_000, hi * 1_000_000 + 999_999);
    add("mac", lo - MAC_EPOCH as i128, hi - MAC_EPOCH as i128);
    add("mac-ns", (lo - MAC_EPOCH as i128) * 1_000_000_000, (hi - MAC_EPOCH as i128) * 1_000_000_000 + 999_999_999);
    add("webkit", (lo + WEBKIT_EPOCH as i128) * 1_000_000, (hi + WEBKIT_EPOCH as i128) * 1_000_000 + 999_999);
    add("filetime", (lo + WEBKIT_EPOCH as i128) * 10_000_000, (hi + WEBKIT_EPOCH as i128) * 10_000_000 + 9_999_999);
    add("hfs+", lo + HFS_EPOCH as i128, hi + HFS_EPOCH as i128);
    v
}

/// A regex matching exactly the decimal integers in [lo, hi].
pub fn num_range_regex(lo: u64, hi: u64) -> String {
    fn same_len(a: &[u8], b: &[u8]) -> String {
        if a.is_empty() {
            return String::new();
        }
        if a == b {
            return String::from_utf8_lossy(a).to_string();
        }
        let n = a.len() - 1;
        let (a0, b0) = (a[0], b[0]);
        let (ar, br) = (&a[1..], &b[1..]);
        if a0 == b0 {
            return format!("{}(?:{})", a0 as char, same_len(ar, br));
        }
        let all = |s: &[u8], c: u8| s.iter().all(|&x| x == c);
        let tail = if n == 0 { String::new() } else { format!("[0-9]{{{n}}}") };
        if all(ar, b'0') && all(br, b'9') {
            return format!("[{}-{}]{tail}", a0 as char, b0 as char);
        }
        let mut parts = Vec::new();
        let nines = vec![b'9'; n];
        let zeros = vec![b'0'; n];
        parts.push(format!("{}(?:{})", a0 as char, same_len(ar, &nines)));
        if b0 - a0 > 1 {
            parts.push(format!("[{}-{}]{tail}", (a0 + 1) as char, (b0 - 1) as char));
        }
        parts.push(format!("{}(?:{})", b0 as char, same_len(&zeros, br)));
        parts.join("|")
    }
    let (a, b) = (lo.to_string(), hi.to_string());
    if a.len() == b.len() {
        return same_len(a.as_bytes(), b.as_bytes());
    }
    // split at each power of ten
    let mut parts = Vec::new();
    let mut cur = lo;
    while cur <= hi {
        let len = cur.to_string().len() as u32;
        let top = 10u64.checked_pow(len).map(|p| p - 1).unwrap_or(u64::MAX).min(hi);
        parts.push(same_len(cur.to_string().as_bytes(), top.to_string().as_bytes()));
        if top == u64::MAX {
            break;
        }
        cur = top + 1;
    }
    parts.join("|")
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;

    #[test]
    fn notes() {
        assert!(timestamp_notes(1_710_460_800.0, false).unwrap().contains("2024-03-15"));
        assert!(timestamp_notes(732_153_600.0, true).unwrap().contains("mac 2024-03-15"));
        assert!(timestamp_notes(13_354_934_400_000_000.0, false).unwrap().contains("webkit 2024-03-15"));
        assert!(timestamp_notes(133_549_344_000_000_000.0, false).unwrap().contains("filetime 2024-03-15"));
        assert!(timestamp_notes(3_793_305_600.0, true).unwrap().contains("hfs+ 2024-03-15"));
    }

    #[test]
    fn ranges_are_exact() {
        for (lo, hi) in [(1_710_460_800u64, 1_710_557_999u64), (5, 1234), (99, 101), (732_100_000, 732_240_000)] {
            let re = Regex::new(&format!("^(?:{})$", num_range_regex(lo, hi))).unwrap();
            for x in [lo.saturating_sub(1), lo, lo + 1, (lo + hi) / 2, hi - 1, hi, hi + 1] {
                assert_eq!(re.is_match(&x.to_string()), (lo..=hi).contains(&x), "{x} in [{lo},{hi}] re={re}");
            }
        }
    }
}
