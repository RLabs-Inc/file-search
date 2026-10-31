//! Builds every encoded "shape" of the needle (UTF-8, UTF-16, legacy code
//! pages, hex, base64, digits, dates, timestamps…). Each shape is its own
//! regex: separately they keep the engine's literal prefilters (GB/s), while
//! one fused alternation would lose them and crawl at ~100 MB/s.

use std::collections::HashMap;
use std::sync::LazyLock;

use base64::Engine;
use chrono::{Datelike, NaiveDate};
use encoding_rs::Encoding;
use regex::bytes::{Regex, RegexBuilder};
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

use crate::formats::time;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Text in a legacy 8-bit / CJK code page.
    Legacy(&'static Encoding),
    /// A number that is a timestamp falling on the searched date.
    Timestamp,
    Text,
    Regex,
    Utf16Le,
    Utf16Be,
    HexText,
    Base64,
    Digits,
    Date,
    HexBytes,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Legacy(e) => match e.name() {
                "windows-1252" => "cp1252",
                "macintosh" => "mac-roman",
                n => n,
            },
            Kind::Timestamp => "timestamp",
            Kind::Text => "text",
            Kind::Regex => "regex",
            Kind::Utf16Le => "utf-16le",
            Kind::Utf16Be => "utf-16be",
            Kind::HexText => "hex-text",
            Kind::Base64 => "base64",
            Kind::Digits => "digits",
            Kind::Date => "date",
            Kind::HexBytes => "bytes",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RawMatch {
    pub start: usize,
    pub end: usize,
    pub kind: Kind,
}

struct Part {
    kind: Kind,
    /// Searched over the whole buffer.
    fast: Option<Regex>,
    /// Patterns whose extra matches always contain one of `triggers`
    /// (accent-folded text, legacy code pages): searched only in windows
    /// around those bytes. Big win on text, where they'd cripple prefilters.
    slow: Option<Regex>,
    triggers: Box<[bool; 256]>,
    /// Max bytes a match can span around a trigger byte.
    reach: usize,
    anchored: Regex,
}

pub struct Matcher {
    parts: Vec<Part>,
    /// Human readable description of each variant, for the run header.
    pub variants: Vec<(Kind, String)>,
}

pub struct MatcherOptions {
    pub case_sensitive: bool,
    pub accent_sensitive: bool,
    pub regex: bool,
    pub hex: bool,
}

impl Matcher {
    pub fn new(needle: &str, opts: &MatcherOptions) -> Result<Matcher, String> {
        if needle.is_empty() {
            return Err("the search string is empty".into());
        }
        let ci = !opts.case_sensitive;
        let fold = Fold { ci, ai: !opts.accent_sensitive, base_only: false };
        let mut pats: Vec<(Kind, String, String)> = Vec::new();

        let hex_needle = parse_hex_needle(needle, opts.hex);
        if opts.hex && hex_needle.is_none() {
            return Err(format!("`{needle}` is not a valid hex byte sequence"));
        }

        if opts.regex {
            let flags = if ci { "(?i)" } else { "" };
            pats.push((Kind::Regex, format!("{flags}(?:{needle})"), format!("/{needle}/")));
        } else if let Some(bytes) = &hex_needle {
            pats.push((Kind::HexBytes, bytes_pattern(bytes), hex_display(bytes)));
            if bytes.len() >= 2 {
                pats.push((Kind::HexText, hex_text_pattern(bytes, false), format!("\"{}\" as text", hex_plain(bytes))));
            }
        } else {
            // 1. plain UTF-8: case/accent-insensitive, NFC or NFD, whitespace-flexible
            pats.push((Kind::Text, text_pattern(needle, &fold), format!("\"{}\"", short(needle))));

            let char_count = needle.chars().filter(|c| !c.is_whitespace()).count();
            // 2. UTF-16 LE / BE
            if char_count >= 2 {
                pats.push((Kind::Utf16Le, utf16_pattern(needle, &fold, false), "UTF-16LE".into()));
                pats.push((Kind::Utf16Be, utf16_pattern(needle, &fold, true), "UTF-16BE".into()));
            }
            // 3. legacy code pages picked from the scripts in the needle
            if char_count >= 2 {
                let mut names = Vec::new();
                for enc in legacy_encodings(needle, &fold) {
                    if let Some(p) = legacy_pattern(needle, &fold, enc) {
                        names.push(Kind::Legacy(enc).label());
                        pats.push((Kind::Legacy(enc), p, String::new()));
                    }
                }
                if !names.is_empty() {
                    let last = pats.len() - 1;
                    pats[last].2 = names.join("/");
                }
            }
            let bytes = needle.as_bytes();
            // 3. the needle's bytes written as hex text ("68656c6c6f", "68 65 6c")
            if bytes.len() >= 3 {
                pats.push((Kind::HexText, hex_text_pattern(bytes, ci), "hex text".into()));
            }
            // 4. base64 at all three alignments
            if bytes.len() >= 4
                && let Some(p) = base64_pattern(needle, ci) {
                    pats.push((Kind::Base64, p, "base64".into()));
                }
            // 5. phone numbers / ids with arbitrary formatting
            if let Some(p) = digits_pattern(needle) {
                pats.push((Kind::Digits, p, "digits, any formatting".into()));
            }
            // 6. dates in many notations
            if let Some((p, n)) = date_pattern(needle) {
                pats.push((Kind::Date, p, format!("{n} date notations")));
            }
            // 7. numeric timestamps of that day, in every epoch we know
            if let Some((p, n)) = timestamp_pattern(needle) {
                pats.push((Kind::Timestamp, p, format!("{n} timestamp epochs")));
            }
        }

        let err = |e: regex::Error| format!("invalid pattern: {e}");
        let mut parts = Vec::new();
        for (kind, p, _) in &pats {
            let anchored = build(&format!(r"\A(?:{p})\z")).map_err(err)?;
            let mut triggers = Box::new([false; 256]);
            let (fast, slow) = match kind {
                Kind::Text if fold.ai => {
                    // pure-ASCII matches are all found by the letter-or-its-base pattern
                    let ascii_fold = text_pattern(needle, &Fold { ci, ai: false, base_only: true });
                    for c in needle.chars() {
                        for v in spelling_chars(c, &fold).into_iter().filter(|v| !v.is_ascii()) {
                            triggers[v.to_string().as_bytes()[0] as usize] = true;
                        }
                    }
                    triggers[0xCC] = true; // combining marks U+0300–U+036F
                    triggers[0xCD] = true;
                    (Some(build(&ascii_fold).map_err(err)?), Some(build(p).map_err(err)?))
                }
                Kind::Legacy(enc) => {
                    // ASCII stretches are the text pattern's job; only the 8-bit bytes matter
                    for c in needle.chars() {
                        for v in spelling_chars(c, &fold) {
                            let vs = v.to_string();
                            if let (b, _, false) = enc.encode(&vs)
                                && let Some(&x) = b.iter().find(|&&x| x >= 0x80) {
                                    triggers[x as usize] = true;
                                }
                        }
                    }
                    (None, Some(build(p).map_err(err)?))
                }
                _ => (Some(build(p).map_err(err)?), None),
            };
            let reach = needle.len() * 6 + 1024;
            parts.push(Part { kind: *kind, fast, slow, triggers, reach, anchored });
        }
        let variants = pats.into_iter().filter(|(_, _, d)| !d.is_empty()).map(|(k, _, d)| (k, d)).collect();
        Ok(Matcher { parts, variants })
    }

    #[inline]
    pub fn is_match(&self, hay: &[u8]) -> bool {
        self.parts.iter().any(|p| {
            p.fast.as_ref().is_some_and(|r| r.is_match(hay))
                || p.slow.as_ref().is_some_and(|r| windows(hay, p.reach, &p.triggers).any(|(a, b)| r.is_match(&hay[a..b])))
        })
    }

    /// Calls `f` for every match, in order, non-overlapping; earlier shapes
    /// win ties (text before UTF-16 before legacy…). Stop by returning `false`.
    pub fn for_each(&self, hay: &[u8], mut f: impl FnMut(RawMatch) -> bool) {
        const MAX_PER_SHAPE: usize = 1_000_000;
        let mut all: Vec<(usize, usize, usize, Kind)> = Vec::new(); // start, prio, end, kind
        for (prio, p) in self.parts.iter().enumerate() {
            if let Some(fast) = &p.fast {
                for m in fast.find_iter(hay).take(MAX_PER_SHAPE) {
                    if m.start() < m.end() {
                        all.push((m.start(), prio, m.end(), p.kind));
                    }
                }
            }
            if let Some(slow) = &p.slow {
                for (a, b) in windows(hay, p.reach, &p.triggers) {
                    for m in slow.find_iter(&hay[a..b]).take(MAX_PER_SHAPE) {
                        if m.start() < m.end() {
                            all.push((a + m.start(), prio, a + m.end(), p.kind));
                        }
                    }
                }
            }
        }
        if all.is_empty() {
            return;
        }
        all.sort_unstable_by_key(|&(s, p, _, _)| (s, p));
        let mut pos = 0;
        for (s, _, e, kind) in all {
            if s < pos {
                continue;
            }
            let (start, end, kind) = self.resolve_utf16(hay, s, e, kind);
            pos = end.max(e);
            if !f(RawMatch { start, end, kind }) {
                break;
            }
        }
    }
}

impl Matcher {
    /// A UTF-16LE string is byte-identical to a UTF-16BE one shifted by a
    /// byte, so pick the reading whose surroundings decode to real text.
    fn resolve_utf16(&self, hay: &[u8], s: usize, e: usize, kind: Kind) -> (usize, usize, Kind) {
        let (alt_kind, alt_s) = match kind {
            Kind::Utf16Be if e < hay.len() => (Kind::Utf16Le, s + 1),
            Kind::Utf16Le if s > 0 => (Kind::Utf16Be, s - 1),
            _ => return (s, e, kind),
        };
        let alt_e = alt_s + (e - s);
        let Some(re) = self.parts.iter().find(|p| p.kind == alt_kind).map(|p| &p.anchored) else { return (s, e, kind) };
        if !re.is_match(&hay[alt_s..alt_e]) {
            return (s, e, kind);
        }
        let (alt, cur) = (utf16_score(hay, alt_s, alt_e, alt_kind == Kind::Utf16Be), utf16_score(hay, s, e, kind == Kind::Utf16Be));
        // on a tie, real UTF-16 text is almost always 2-byte aligned
        if alt > cur || (alt == cur && alt_s % 2 == 0 && s % 2 == 1) {
            (alt_s, alt_e, alt_kind)
        } else {
            (s, e, kind)
        }
    }
}

fn utf16_score(hay: &[u8], s: usize, e: usize, be: bool) -> usize {
    let plausible = |u: u16| matches!(u, 0x09 | 0x0A | 0x0D | 0x20..=0x7E | 0xA0..=0x24F);
    let unit = |i: usize| -> Option<u16> {
        let b = hay.get(i..i + 2)?;
        Some(if be { u16::from_be_bytes([b[0], b[1]]) } else { u16::from_le_bytes([b[0], b[1]]) })
    };
    let mut score = 0;
    for k in 1..=16usize {
        if let Some(u) = s.checked_sub(2 * k).and_then(unit) {
            score += plausible(u) as usize;
        }
        if let Some(u) = unit(e + 2 * (k - 1)) {
            score += plausible(u) as usize;
        }
    }
    score
}

/// Merged windows of ±`reach` bytes around every trigger byte.
fn windows<'a>(hay: &'a [u8], reach: usize, trig: &'a [bool; 256]) -> impl Iterator<Item = (usize, usize)> + 'a {
    let mut i = 0;
    std::iter::from_fn(move || {
        loop {
            // triggers are all ≥ 0x80: skip ASCII 8 bytes at a time
            while i + 8 <= hay.len() && u64::from_ne_bytes(hay[i..i + 8].try_into().unwrap()) & 0x8080_8080_8080_8080 == 0 {
                i += 8;
            }
            if i >= hay.len() {
                return None;
            }
            if trig[hay[i] as usize] {
                break;
            }
            i += 1;
        }
        let start = i.saturating_sub(reach);
        let mut end = (i + reach).min(hay.len());
        // extend while more triggers fall inside the window
        let mut j = i + 1;
        while j < end {
            if trig[hay[j] as usize] {
                end = (j + reach).min(hay.len());
            }
            j += 1;
        }
        i = end;
        Some((start, end))
    })
}

fn build(p: &str) -> Result<Regex, regex::Error> {
    RegexBuilder::new(p)
        .size_limit(512 << 20)
        .dfa_size_limit(512 << 20)
        .build()
}

fn short(s: &str) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 60 {
        format!("{}…", one_line.chars().take(59).collect::<String>())
    } else {
        one_line
    }
}

// ── character folding ───────────────────────────────────────────────────

pub struct Fold {
    pub ci: bool,
    pub ai: bool,
    /// Only the letter itself and its unaccented base (the ASCII-fast form).
    pub base_only: bool,
}

/// Letters NFD can't decompose but people type without the accent anyway.
const MANUAL_BASE: &[(char, char)] = &[
    ('ø', 'o'), ('Ø', 'O'), ('đ', 'd'), ('Đ', 'D'), ('ł', 'l'), ('Ł', 'L'), ('ı', 'i'), ('ħ', 'h'), ('Ħ', 'H'),
    ('ŧ', 't'), ('Ŧ', 'T'),
];

fn base_char(c: char) -> char {
    if let Some(&(_, b)) = MANUAL_BASE.iter().find(|(x, _)| *x == c) {
        return b;
    }
    let mut it = c.nfd();
    let first = it.next().unwrap_or(c);
    if it.all(is_combining_mark) { first } else { c }
}

/// lowercase base letter → every precomposed letter built on it (both cases)
static ACCENTED: LazyLock<HashMap<char, Vec<char>>> = LazyLock::new(|| {
    let mut m: HashMap<char, Vec<char>> = HashMap::new();
    for range in [0xC0..=0x24F, 0x1E00..=0x1EFF, 0x370..=0x3FF, 0x400..=0x4FF] {
        for c in range.filter_map(char::from_u32) {
            let b = base_char(c);
            if b != c {
                m.entry(lower1(b)).or_default().push(c);
            }
        }
    }
    m
});

fn lower1(c: char) -> char {
    let mut l = c.to_lowercase();
    match (l.next(), l.next()) {
        (Some(x), None) => x,
        _ => c,
    }
}

fn upper1(c: char) -> Option<char> {
    let mut u = c.to_uppercase();
    match (u.next(), u.next()) {
        (Some(x), None) => Some(x),
        _ => None,
    }
}

/// Every single-character spelling of `c` we accept.
fn spelling_chars(c: char, f: &Fold) -> Vec<char> {
    let mut chars = vec![c];
    let push = |x: char, v: &mut Vec<char>| {
        if !v.contains(&x) {
            v.push(x)
        }
    };
    if f.base_only && c.is_alphabetic() {
        push(base_char(c), &mut chars);
    }
    if f.ai && c.is_alphabetic() {
        let b = base_char(c);
        let mut cand = vec![b];
        cand.extend(ACCENTED.get(&lower1(b)).into_iter().flatten().copied());
        for x in cand {
            if f.ci || x.is_uppercase() == c.is_uppercase() {
                push(x, &mut chars);
            }
        }
    }
    if f.ci {
        for x in chars.clone() {
            push(lower1(x), &mut chars);
            if let Some(u) = upper1(x) {
                push(u, &mut chars);
            }
        }
    }
    chars
}

/// Spellings as strings, including decomposed (NFD) sequences when accents matter.
fn spellings(c: char, f: &Fold) -> Vec<String> {
    let chars = spelling_chars(c, f);
    let mut out: Vec<String> = chars.iter().map(|c| c.to_string()).collect();
    if !f.ai {
        for c in &chars {
            let d: String = c.to_string().nfd().collect();
            if d.chars().count() > 1 && !out.contains(&d) {
                out.push(d);
            }
        }
    }
    out
}

// ── UTF-8 text ──────────────────────────────────────────────────────────

fn text_pattern(needle: &str, f: &Fold) -> String {
    let mut body = String::new();
    let mut in_ws = false;
    let chars: Vec<char> = needle.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c.is_whitespace() {
            if !in_ws {
                // keep leading/trailing whitespace strict; inner runs also match _ and -
                let edge = i == 0 || chars[i..].iter().all(|c| c.is_whitespace());
                body.push_str(if edge { r"\s+" } else { r"[\s_\-]+" });
                in_ws = true;
            }
            continue;
        }
        in_ws = false;
        let alts: Vec<String> = spellings(c, f).iter().map(|s| regex::escape(s)).collect();
        if alts.len() == 1 {
            body.push_str(&alts[0]);
        } else {
            body.push_str(&format!("(?:{})", alts.join("|")));
        }
        if f.ai && c.is_alphabetic() {
            body.push_str(r"\p{M}*"); // decomposed accents (macOS file names!)
        }
    }
    if f.ci { format!("(?i:{body})") } else { body }
}

// ── UTF-16 ──────────────────────────────────────────────────────────────

fn utf16_units(s: &str, be: bool) -> String {
    let mut out = String::new();
    for u in s.encode_utf16() {
        let [a, b] = if be { u.to_be_bytes() } else { u.to_le_bytes() };
        out.push_str(&format!(r"\x{a:02X}\x{b:02X}"));
    }
    out
}

fn utf16_pattern(needle: &str, f: &Fold, be: bool) -> String {
    let (ws, marks) = if be {
        (r"(?:\x00[\x09\x0A\x0D\x20])+", r"(?:\x03[\x00-\x6F])*")
    } else {
        (r"(?:[\x09\x0A\x0D\x20]\x00)+", r"(?:[\x00-\x6F]\x03)*")
    };
    let mut out = String::from("(?-u:");
    let mut in_ws = false;
    for c in needle.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push_str(ws);
                in_ws = true;
            }
            continue;
        }
        in_ws = false;
        let alts: Vec<String> = spellings(c, f).iter().map(|v| utf16_units(v, be)).collect();
        if alts.len() == 1 {
            out.push_str(&alts[0]);
        } else {
            out.push_str(&format!("(?:{})", alts.join("|")));
        }
        if f.ai && c.is_alphabetic() {
            out.push_str(marks);
        }
    }
    out.push(')');
    out
}

// ── legacy code pages ───────────────────────────────────────────────────

fn legacy_encodings(needle: &str, f: &Fold) -> Vec<&'static Encoding> {
    use encoding_rs::*;
    let any = |r: std::ops::RangeInclusive<u32>| needle.chars().any(|c| r.contains(&(c as u32)));
    let any_of = |set: &str| needle.chars().any(|c| set.contains(c));
    let mut v: Vec<&'static Encoding> = Vec::new();
    let western = needle.chars().any(|c| !c.is_ascii() && c.is_alphabetic() && (c as u32) < 0x250)
        || (f.ai && needle.chars().any(|c| c.is_ascii_alphabetic()));
    if western {
        v.extend([WINDOWS_1252, MACINTOSH]);
    }
    if any_of("ąćęłńśźżĄĆĘŁŃŚŹŻčďěňřšťůžČĎĚŇŘŠŤŮŽőűŐŰĺľŕĹĽŔ") {
        v.extend([WINDOWS_1250, ISO_8859_2]);
    }
    if any_of("ğĞşŞıİ") {
        v.push(WINDOWS_1254);
    }
    if any(0x0400..=0x04FF) {
        v.extend([WINDOWS_1251, KOI8_R, KOI8_U, IBM866]);
    }
    if any(0x0370..=0x03FF) {
        v.extend([WINDOWS_1253, ISO_8859_7]);
    }
    if any(0x0590..=0x05FF) {
        v.push(WINDOWS_1255);
    }
    if any(0x0600..=0x06FF) {
        v.push(WINDOWS_1256);
    }
    if any(0x0E00..=0x0E7F) {
        v.push(WINDOWS_874);
    }
    if any(0x1EA0..=0x1EF9) {
        v.push(WINDOWS_1258);
    }
    if any(0x3040..=0x30FF) || any(0x4E00..=0x9FFF) {
        v.extend([SHIFT_JIS, EUC_JP]);
    }
    if any(0x4E00..=0x9FFF) {
        v.extend([GB18030, BIG5]);
    }
    if any(0xAC00..=0xD7AF) {
        v.push(EUC_KR);
    }
    v
}

fn legacy_pattern(needle: &str, f: &Fold, enc: &'static Encoding) -> Option<String> {
    let esc = |b: &[u8]| b.iter().map(|x| format!(r"\x{x:02X}")).collect::<String>();
    let mut out = String::from("(?-u:");
    let mut in_ws = false;
    let mut non_ascii = false;
    for c in needle.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push_str(r"[\x09\x0A\x0D\x20]+");
                in_ws = true;
            }
            continue;
        }
        in_ws = false;
        let mut alts: Vec<String> = Vec::new();
        for ch in spelling_chars(c, f) {
            let chs = ch.to_string();
            let (bytes, _, err) = enc.encode(&chs);
            if !err {
                non_ascii |= bytes.iter().any(|b| *b >= 0x80);
                let e = esc(&bytes);
                if !alts.contains(&e) {
                    alts.push(e);
                }
            }
        }
        if alts.is_empty() {
            return None; // this code page can't spell the needle
        }
        out.push_str(&format!("(?:{})", alts.join("|")));
    }
    out.push(')');
    // identical to the UTF-8 pattern for pure ASCII: nothing to add
    if non_ascii { Some(out) } else { None }
}

// ── hex ─────────────────────────────────────────────────────────────────

fn hex_byte_alt(b: u8, ci: bool) -> String {
    let lo = b.to_ascii_lowercase();
    let up = b.to_ascii_uppercase();
    if ci && lo != up {
        format!("(?:{lo:02x}|{up:02x})")
    } else {
        format!("{b:02x}")
    }
}

fn hex_text_pattern(bytes: &[u8], ci: bool) -> String {
    let parts: Vec<String> = bytes.iter().map(|&b| hex_byte_alt(b, ci)).collect();
    // hex digits themselves are matched case-insensitively; allow "de ad", "de:ad", "dead"
    format!("(?i-u:{})", parts.join(r"[ :\-]?"))
}

fn bytes_pattern(bytes: &[u8]) -> String {
    let mut s = String::from("(?-u:");
    for b in bytes {
        s.push_str(&format!(r"\x{b:02X}"));
    }
    s.push(')');
    s
}

fn hex_plain(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_display(bytes: &[u8]) -> String {
    let v: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
    format!("bytes {}", v.join(" "))
}

/// `0xDEADBEEF`, `hex:de ad be ef`, or anything with `--hex`.
fn parse_hex_needle(needle: &str, forced: bool) -> Option<Vec<u8>> {
    let t = needle.trim();
    let body = if let Some(r) = t.strip_prefix("hex:") {
        r
    } else if let Some(r) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        r
    } else if forced {
        t
    } else {
        return None;
    };
    let digits: String = body
        .chars()
        .filter(|c| !matches!(c, ' ' | ':' | '-' | ',' | '\t'))
        .collect::<String>()
        .replace("0x", "")
        .replace("\\x", "");
    if digits.is_empty() || !digits.len().is_multiple_of(2) || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    (0..digits.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).ok())
        .collect()
}

// ── base64 ──────────────────────────────────────────────────────────────

fn base64_pattern(needle: &str, ci: bool) -> Option<String> {
    let mut forms: Vec<String> = vec![needle.to_string()];
    if ci {
        for f in [needle.to_lowercase(), needle.to_uppercase(), title_case(needle), capitalized(needle)] {
            if !forms.contains(&f) {
                forms.push(f);
            }
        }
    }
    let mut alts = Vec::new();
    for f in &forms {
        let b = f.as_bytes();
        for o in 0..3usize {
            let mut buf = vec![0u8; o];
            buf.extend_from_slice(b);
            let enc = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&buf);
            let skip = (8 * o).div_ceil(6);
            let keep = (8 * (o + b.len())) / 6;
            if keep <= skip || keep - skip < 4 {
                continue;
            }
            let core: String = enc.chars().skip(skip).take(keep - skip).collect();
            let mut p = String::new();
            for ch in core.chars() {
                match ch {
                    '+' => p.push_str(r"[+\-]"),
                    '/' => p.push_str(r"[/_]"),
                    c => p.push(c),
                }
            }
            if !alts.contains(&p) {
                alts.push(p);
            }
        }
    }
    if alts.is_empty() {
        None
    } else {
        Some(format!("(?-u:{})", alts.join("|")))
    }
}

fn title_case(s: &str) -> String {
    let mut out = String::new();
    let mut start = true;
    for c in s.chars() {
        if start { out.extend(c.to_uppercase()) } else { out.extend(c.to_lowercase()) }
        start = c.is_whitespace();
    }
    out
}

fn capitalized(s: &str) -> String {
    let low = s.to_lowercase();
    let mut cs = low.chars();
    match cs.next() {
        Some(f) => f.to_uppercase().chain(cs).collect(),
        None => low,
    }
}

// ── digits ──────────────────────────────────────────────────────────────

fn digits_pattern(needle: &str) -> Option<String> {
    let t = needle.trim();
    if !t.chars().all(|c| c.is_ascii_digit() || " -.()/+".contains(c)) {
        return None;
    }
    let digits: Vec<char> = t.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 6 {
        return None;
    }
    let body: Vec<String> = digits.iter().map(|d| d.to_string()).collect();
    Some(format!(r"(?-u:{})", body.join(r"[ \-.()/]{0,3}")))
}

// ── dates ───────────────────────────────────────────────────────────────

const EN_MONTHS: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September",
    "October", "November", "December",
];
const PT_MONTHS: [&str; 12] = [
    "janeiro", "fevereiro", "março", "abril", "maio", "junho", "julho", "agosto", "setembro",
    "outubro", "novembro", "dezembro",
];

pub fn parse_dates(needle: &str) -> Vec<NaiveDate> {
    let t = needle.trim();
    let mut out: Vec<NaiveDate> = Vec::new();
    let mut push = |d: NaiveDate| {
        if (1900..=2200).contains(&d.year()) && !out.contains(&d) {
            out.push(d);
        }
    };
    let numeric = [
        "%Y-%m-%d", "%Y/%m/%d", "%Y.%m.%d", "%d/%m/%Y", "%m/%d/%Y", "%d-%m-%Y", "%m-%d-%Y",
        "%d.%m.%Y", "%B %d, %Y", "%B %d %Y", "%b %d, %Y", "%b %d %Y", "%d %B %Y", "%d %b %Y",
    ];
    for f in numeric {
        if let Ok(d) = NaiveDate::parse_from_str(t, f) {
            push(d);
        }
    }
    if t.len() == 8 && t.chars().all(|c| c.is_ascii_digit())
        && let Ok(d) = NaiveDate::parse_from_str(t, "%Y%m%d") {
            push(d);
        }
    // "15 de março de 2024"
    let low = t.to_lowercase();
    for (i, m) in PT_MONTHS.iter().enumerate() {
        if low.contains(m) || low.contains(&m[..3]) {
            let nums: Vec<u32> = low
                .split(|c: char| !c.is_ascii_digit())
                .filter_map(|x| x.parse().ok())
                .collect();
            if nums.len() == 2 {
                let (d, y) = if nums[0] > 31 { (nums[1], nums[0]) } else { (nums[0], nums[1]) };
                if let Some(date) = NaiveDate::from_ymd_opt(y as i32, i as u32 + 1, d) {
                    push(date);
                }
            }
            break;
        }
    }
    out
}

fn date_pattern(needle: &str) -> Option<(String, usize)> {
    let dates = parse_dates(needle);
    if dates.is_empty() {
        return None;
    }
    let mut alts: Vec<String> = Vec::new();
    for d in dates {
        let (y, m, dd) = (d.year(), d.month(), d.day());
        let mi = (m - 1) as usize;
        let en = EN_MONTHS[mi];
        let en3 = &en[..3];
        let pt = PT_MONTHS[mi];
        let pt3: String = pt.chars().take(3).collect();
        let mut v = vec![
            format!("{y:04}-{m:02}-{dd:02}"),
            format!("{y:04}/{m:02}/{dd:02}"),
            format!("{y:04}.{m:02}.{dd:02}"),
            format!("{y:04}{m:02}{dd:02}"),
            format!("{y:04}:{m:02}:{dd:02}"), // EXIF
            format!("{dd:02}/{m:02}/{y:04}"),
            format!("{dd}/{m}/{y:04}"),
            format!("{m:02}/{dd:02}/{y:04}"),
            format!("{m}/{dd}/{y:04}"),
            format!("{dd:02}-{m:02}-{y:04}"),
            format!("{dd:02}.{m:02}.{y:04}"),
            format!("{dd}.{m}.{y:04}"),
            format!("{en} {dd}, {y}"),
            format!("{en} {dd} {y}"),
            format!("{en3} {dd}, {y}"),
            format!("{en3} {dd} {y}"),
            format!("{dd} {en} {y}"),
            format!("{dd} {en3} {y}"),
            format!("{dd:02} {en3} {y}"),
            format!("{dd}-{en3}-{y}"),
            format!("{dd} de {pt} de {y}"),
            format!("{dd} {pt3} {y}"),
            format!("{dd}/{pt3}/{y}"),
        ];
        if dd < 10 {
            v.push(format!("{en3}  {dd} {y}")); // ctime style padding
        }
        for s in v {
            let p = regex::escape(&s).replace(' ', r"\s+");
            if !alts.contains(&p) {
                alts.push(p);
            }
        }
    }
    let n = alts.len();
    Some((format!("(?i:{})", alts.join("|")), n))
}

fn timestamp_pattern(needle: &str) -> Option<(String, usize)> {
    let dates = parse_dates(needle);
    if dates.is_empty() {
        return None;
    }
    let mut alts = Vec::new();
    let mut epochs = Vec::new();
    for d in dates {
        let (lo, hi) = time::day_range(d);
        for (label, a, b) in time::epoch_ranges(lo, hi) {
            alts.push(time::num_range_regex(a, b));
            if !epochs.contains(&label) {
                epochs.push(label);
            }
        }
    }
    Some((format!(r"(?-u:\b(?:{})\b)", alts.join("|")), epochs.len()))
}

// ── tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(m: &Matcher, hay: &[u8]) -> Vec<Kind> {
        let mut v = Vec::new();
        m.for_each(hay, |r| {
            v.push(r.kind);
            true
        });
        v
    }

    fn opts() -> MatcherOptions {
        MatcherOptions { case_sensitive: false, accent_sensitive: false, regex: false, hex: false }
    }

    #[test]
    fn accents_nfd_and_legacy() {
        let m = Matcher::new("São Paulo", &opts()).unwrap();
        // accent-insensitive both ways
        assert_eq!(kinds(&m, b"cidade de Sao Paulo"), vec![Kind::Text]);
        let m2 = Matcher::new("Sao Paulo", &opts()).unwrap();
        assert_eq!(kinds(&m2, "SÃO PAULO".as_bytes()), vec![Kind::Text]);
        // decomposed (macOS file names)
        let nfd: String = "São Paulo".nfd().collect();
        assert_eq!(kinds(&m, nfd.as_bytes()), vec![Kind::Text]);
        let nfd16: Vec<u8> = nfd.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(kinds(&m, &nfd16), vec![Kind::Utf16Le]);
        // legacy code pages
        assert_eq!(kinds(&m, b"cidade de S\xe3o Paulo"), vec![Kind::Legacy(encoding_rs::WINDOWS_1252)]);
        assert_eq!(kinds(&m2, b"S\x8Bo Paulo"), vec![Kind::Legacy(encoding_rs::MACINTOSH)]);
        let m3 = Matcher::new("Привет мир", &opts()).unwrap();
        let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode("ну привет мир!");
        assert_eq!(kinds(&m3, &cp1251), vec![Kind::Legacy(encoding_rs::WINDOWS_1251)]);
        let m4 = Matcher::new("東京タワー", &opts()).unwrap();
        let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode("ここは東京タワーです");
        assert_eq!(kinds(&m4, &sjis), vec![Kind::Legacy(encoding_rs::SHIFT_JIS)]);
    }

    #[test]
    fn accent_sensitive_still_matches_nfd() {
        let o = MatcherOptions { accent_sensitive: true, ..opts() };
        let m = Matcher::new("São", &o).unwrap();
        assert!(kinds(&m, b"Sao").is_empty());
        let nfd: String = "São".nfd().collect();
        assert_eq!(kinds(&m, nfd.as_bytes()), vec![Kind::Text]);
    }

    #[test]
    fn timestamps_of_a_day() {
        let m = Matcher::new("2024-03-15", &opts()).unwrap();
        // unix seconds / ms, Mac absolute, Chrome µs, inside JSON
        let k = kinds(&m, br#"{"a":1710496800,"b":1710496800123,"c":732196800.5,"d":13354970400000000,"e":17104968001}"#);
        assert_eq!(k, vec![Kind::Timestamp; 4]);
        assert_eq!(kinds(&m, b"photo taken 2024:03:15 10:00:00"), vec![Kind::Date]);
    }

    #[test]
    fn finds_all_shapes() {
        let m = Matcher::new("Hello World", &opts()).unwrap();
        assert_eq!(kinds(&m, b"say hello\n  world!"), vec![Kind::Text]);
        let le: Vec<u8> = "xxHELLO world".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(kinds(&m, &le), vec![Kind::Utf16Le]);
        let be: Vec<u8> = "Hello World".encode_utf16().flat_map(|u| u.to_be_bytes()).collect();
        assert_eq!(kinds(&m, &be), vec![Kind::Utf16Be]);
        assert_eq!(kinds(&m, b"0x48656c6c6f20576f726c64"), vec![Kind::HexText]);
        for pre in ["", "a", "ab"] {
            let enc = base64::engine::general_purpose::STANDARD.encode(format!("{pre}Hello World!!"));
            assert_eq!(kinds(&m, enc.as_bytes()), vec![Kind::Base64], "prefix {pre:?} -> {enc}");
        }
    }

    #[test]
    fn phone_numbers() {
        let m = Matcher::new("11987654321", &opts()).unwrap();
        assert_eq!(kinds(&m, b"tel: (11) 98765-4321"), vec![Kind::Digits]);
        assert_eq!(kinds(&m, b"11987654321"), vec![Kind::Text]);
    }

    #[test]
    fn dates() {
        let m = Matcher::new("2024-03-15", &opts()).unwrap();
        assert_eq!(kinds(&m, b"on 15/03/2024 and March 15, 2024 and 15 de mar\xc3\xa7o de 2024"), vec![Kind::Date; 3]);
    }

    #[test]
    fn hex_bytes() {
        let m = Matcher::new("0xDEADBEEF", &opts()).unwrap();
        assert_eq!(kinds(&m, &[0, 0xde, 0xad, 0xbe, 0xef, 1]), vec![Kind::HexBytes]);
        assert_eq!(kinds(&m, b"de:ad:be:ef"), vec![Kind::HexText]);
    }

    #[test]
    fn unicode_case() {
        let m = Matcher::new("São Paulo", &opts()).unwrap();
        assert_eq!(kinds(&m, "SÃO PAULO".as_bytes()), vec![Kind::Text]);
        let le: Vec<u8> = "SÃO PAULO".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(kinds(&m, &le), vec![Kind::Utf16Le]);
    }
}


#[cfg(test)]
mod bench {
    use super::*;
    use crate::formats::{compress, text};
    #[test]
    #[ignore]
    fn stages() {
        let hay = std::fs::read(std::env::var("BENCH_FILE").unwrap()).unwrap();
        // split into ~10 KB "files" like the real tree
        let files: Vec<&[u8]> = hay.chunks(10_000).collect();
        let mbs = |t: std::time::Instant| format!("{:>7.0} MB/s", hay.len() as f64 / t.elapsed().as_secs_f64() / 1e6);
        for (label, ai) in [("accent-insensitive", false), ("accent-sensitive", true)] {
            let o = MatcherOptions { case_sensitive: false, accent_sensitive: ai, regex: false, hex: false };
            let m = Matcher::new("phoenix protocol", &o).unwrap();
            let t = std::time::Instant::now();
            for f in &files { m.for_each(f, |_| true); }
            eprintln!("for_each raw  [{label}] {}", mbs(t));
            let views: Vec<_> = files.iter().map(|f| text::derived_views(f)).collect();
            let t = std::time::Instant::now();
            for vs in &views { for v in vs { m.for_each(&v.data, |_| true); } }
            eprintln!("for_each views[{label}] {}  ({} views)", mbs(t), views.iter().map(|v| v.len()).sum::<usize>());
        }
        let t = std::time::Instant::now();
        for f in &files { std::hint::black_box(text::derived_views(f)); }
        eprintln!("derived_views         {}", mbs(t));
        let t = std::time::Instant::now();
        for f in &files { std::hint::black_box(text::base64_blobs(f, 64, 5000)); }
        eprintln!("base64_blobs          {}", mbs(t));
        let t = std::time::Instant::now();
        for f in &files { std::hint::black_box(compress::carve_gzip(f, 1 << 20, 100)); }
        eprintln!("carve_gzip            {}", mbs(t));
    }
}
