//! Builds every encoded "shape" of the needle and fuses them into one regex,
//! so each buffer is scanned in a single pass no matter how many encodings
//! we look for.

use base64::Engine;
use chrono::{Datelike, NaiveDate};
use regex::bytes::{Regex, RegexBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
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

pub struct Matcher {
    combined: Regex,
    parts: Vec<(Kind, Regex)>,
    /// Human readable description of each variant, for the run header.
    pub variants: Vec<(Kind, String)>,
}

pub struct MatcherOptions {
    pub case_sensitive: bool,
    pub regex: bool,
    pub hex: bool,
}

impl Matcher {
    pub fn new(needle: &str, opts: &MatcherOptions) -> Result<Matcher, String> {
        if needle.is_empty() {
            return Err("the search string is empty".into());
        }
        let ci = !opts.case_sensitive;
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
            // 1. plain UTF-8 (whitespace-flexible so paragraphs survive re-wrapping)
            pats.push((Kind::Text, text_pattern(needle, ci), format!("\"{}\"", short(needle))));

            let char_count = needle.chars().filter(|c| !c.is_whitespace()).count();
            // 2. UTF-16 LE / BE
            if char_count >= 2 {
                pats.push((Kind::Utf16Le, utf16_pattern(needle, ci, false), "UTF-16LE".into()));
                pats.push((Kind::Utf16Be, utf16_pattern(needle, ci, true), "UTF-16BE".into()));
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
        }

        let mut parts = Vec::new();
        let mut alts = Vec::new();
        for (kind, p, _) in &pats {
            let anchored = build(&format!(r"\A(?:{p})\z"))
                .map_err(|e| format!("invalid pattern: {e}"))?;
            parts.push((*kind, anchored));
            alts.push(format!("(?:{p})"));
        }
        let combined = build(&alts.join("|")).map_err(|e| format!("invalid pattern: {e}"))?;
        let variants = pats.into_iter().map(|(k, _, d)| (k, d)).collect();
        Ok(Matcher { combined, parts, variants })
    }

    #[inline]
    pub fn is_match(&self, hay: &[u8]) -> bool {
        self.combined.is_match(hay)
    }

    /// Calls `f` for every match; stop early by returning `false`.
    pub fn for_each(&self, hay: &[u8], mut f: impl FnMut(RawMatch) -> bool) {
        for m in self.combined.find_iter(hay) {
            if m.start() == m.end() {
                continue;
            }
            let slice = &hay[m.start()..m.end()];
            let kind = self
                .parts
                .iter()
                .find(|(_, r)| r.is_match(slice))
                .map(|(k, _)| *k)
                .unwrap_or(Kind::Text);
            let (start, end, kind) = self.resolve_utf16(hay, m.start(), m.end(), kind);
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
        let Some((_, re)) = self.parts.iter().find(|(k, _)| *k == alt_kind) else { return (s, e, kind) };
        if !re.is_match(&hay[alt_s..alt_e]) {
            return (s, e, kind);
        }
        if utf16_score(hay, alt_s, alt_e, alt_kind == Kind::Utf16Be) > utf16_score(hay, s, e, kind == Kind::Utf16Be) {
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

// ── UTF-8 text ──────────────────────────────────────────────────────────

fn text_pattern(needle: &str, ci: bool) -> String {
    let segs: Vec<String> = needle.split_whitespace().map(regex::escape).collect();
    let mut body = segs.join(r"[\s_\-]+");
    // keep leading / trailing whitespace if the user typed it
    if needle.starts_with(char::is_whitespace) {
        body = format!(r"\s+{body}");
    }
    if needle.ends_with(char::is_whitespace) && !segs.is_empty() {
        body.push_str(r"\s+");
    }
    if ci { format!("(?i:{body})") } else { body }
}

// ── UTF-16 ──────────────────────────────────────────────────────────────

fn utf16_units(c: char, be: bool) -> String {
    let mut buf = [0u16; 2];
    let mut out = String::new();
    for u in c.encode_utf16(&mut buf) {
        let [a, b] = if be { u.to_be_bytes() } else { u.to_le_bytes() };
        out.push_str(&format!(r"\x{a:02X}\x{b:02X}"));
    }
    out
}

fn case_variants(c: char, ci: bool) -> Vec<char> {
    let mut v = vec![c];
    if ci {
        for x in c.to_lowercase().chain(c.to_uppercase()) {
            if !v.contains(&x) && x.to_lowercase().count() == 1 {
                v.push(x);
            }
        }
    }
    v
}

fn utf16_pattern(needle: &str, ci: bool, be: bool) -> String {
    let ws = if be {
        r"(?:\x00[\x09\x0A\x0D\x20])+"
    } else {
        r"(?:[\x09\x0A\x0D\x20]\x00)+"
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
        let vars = case_variants(c, ci);
        if vars.len() == 1 {
            out.push_str(&utf16_units(c, be));
        } else {
            let alts: Vec<String> = vars.iter().map(|&v| utf16_units(v, be)).collect();
            out.push_str(&format!("(?:{})", alts.join("|")));
        }
    }
    out.push(')');
    out
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
        MatcherOptions { case_sensitive: false, regex: false, hex: false }
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
