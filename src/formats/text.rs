//! Derived text views: transformed copies of a text buffer (markup stripped,
//! URL decoded, …) plus a map from every output byte back to its original
//! offset, so hits can be de-duplicated against the raw scan.

use std::sync::LazyLock;

use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use regex::bytes::Regex;

// ── text heuristics ─────────────────────────────────────────────────────

pub fn is_texty(d: &[u8]) -> bool {
    let sample = &d[..d.len().min(8192)];
    !sample.is_empty() && !sample.contains(&0)
}

/// A transformed view of a buffer with a byte→original-offset map.
pub struct View {
    pub name: &'static str,
    pub data: Vec<u8>,
    pub map: Vec<u32>,
}

impl View {
    fn new(name: &'static str, cap: usize) -> View {
        View { name, data: Vec::with_capacity(cap), map: Vec::with_capacity(cap) }
    }
    fn push(&mut self, b: u8, orig: usize) {
        self.data.push(b);
        self.map.push(orig as u32);
    }
    fn push_char(&mut self, c: char, orig: usize) {
        let mut tmp = [0u8; 4];
        for b in c.encode_utf8(&mut tmp).bytes() {
            self.push(b, orig);
        }
    }
    /// Original byte range covered by view range [s, e).
    pub fn orig_range(&self, s: usize, e: usize) -> (usize, usize) {
        let a = self.map[s] as usize;
        let b = self.map[e.saturating_sub(1).max(s)] as usize + 1;
        (a, b.max(a + 1))
    }
}

static RE_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[A-Za-z/!?][^<>]{0,400}>").unwrap());
static RE_PCT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"%[0-9A-Fa-f]{2}").unwrap());
static RE_ESC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\\(?:u[0-9A-Fa-f]{4}|x[0-9A-Fa-f]{2})").unwrap());
static RE_QP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)quoted-printable").unwrap());
static RE_EW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"=\?([A-Za-z0-9_\-:.]+)(?:\*[A-Za-z\-]+)?\?([BbQq])\?([^?\s]*)\?=").unwrap());
static RE_PDF_TEXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\)\s*Tj|\]\s*TJ|>\s*Tj").unwrap());

/// All derived text views worth building for this buffer.
pub fn derived_views(d: &[u8]) -> Vec<View> {
    let mut out = Vec::new();
    if RE_TAG.is_match(d) {
        out.push(strip_markup(d));
    }
    if RE_QP.is_match(d) {
        out.push(quoted_printable(d));
    }
    if RE_PCT.find_iter(d).take(2).count() >= 1 {
        out.push(percent_decode(d));
    }
    if RE_ESC.is_match(d) {
        out.push(unescape(d));
    }
    if RE_PDF_TEXT.is_match(d) {
        out.push(pdf_text(d));
    }
    if RE_EW.is_match(d) {
        out.push(mime_words(d));
    }
    if d.starts_with(b"{\\rtf") {
        out.push(rtf_text(d));
    }
    out
}

const BLOCK_TAGS: &[&str] = &[
    "p", "br", "div", "li", "tr", "td", "th", "h1", "h2", "h3", "h4", "h5", "h6", "w:p", "w:br",
    "w:tab", "w:cr", "text:p", "text:s", "text:tab", "text:line-break", "a:p", "a:br", "string",
    "key", "integer", "real", "date", "si", "row", "c", "v", "t", "title", "section", "body",
    "table", "ul", "ol", "dd", "dt", "hr", "blockquote", "pre",
];

fn strip_markup(d: &[u8]) -> View {
    let mut v = View::new("markup-stripped", d.len());
    let mut i = 0;
    let mut last_space = false;
    while i < d.len() {
        // copy the plain run up to the next '<' or '&' in one go
        let run = d[i..].iter().position(|&c| c == b'<' || c == b'&').unwrap_or(d.len() - i);
        if run > 0 {
            v.data.extend_from_slice(&d[i..i + run]);
            v.map.extend(i as u32..(i + run) as u32);
            last_space = d[i + run - 1].is_ascii_whitespace();
            i += run;
            continue;
        }
        let b = d[i];
        if b == b'<'
            && let Some(tag_end) = tag_at(d, i) {
                let tag = &d[i + 1..tag_end - 1];
                let name: Vec<u8> = tag
                    .iter()
                    .skip_while(|&&c| c == b'/')
                    .take_while(|&&c| !c.is_ascii_whitespace() && c != b'/' && c != b'>')
                    .map(|c| c.to_ascii_lowercase())
                    .collect();
                let name = String::from_utf8_lossy(&name);
                if BLOCK_TAGS.contains(&name.as_ref()) && !last_space {
                    v.push(b' ', i);
                    last_space = true;
                }
                i = tag_end;
                continue;
            }
        if b == b'&'
            && let Some((c, len)) = entity(&d[i..]) {
                v.push_char(c, i);
                last_space = c == ' ';
                i += len;
                continue;
            }
        last_space = b.is_ascii_whitespace();
        v.push(b, i);
        i += 1;
    }
    v
}

/// Same rule as RE_TAG, anchored at `i`: returns the index just past '>'.
fn tag_at(d: &[u8], i: usize) -> Option<usize> {
    if !matches!(d.get(i + 1), Some(c) if c.is_ascii_alphabetic() || matches!(c, b'/' | b'!' | b'?')) {
        return None;
    }
    let lim = (i + 2 + 400).min(d.len());
    let rel = d.get(i + 2..lim)?.iter().position(|&c| c == b'>' || c == b'<')?;
    (d[i + 2 + rel] == b'>').then_some(i + 3 + rel)
}

fn entity(s: &[u8]) -> Option<(char, usize)> {
    let end = s.iter().take(12).position(|&c| c == b';')?;
    let name = std::str::from_utf8(&s[1..end]).ok()?;
    let c = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        _ => {
            let n = if let Some(h) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                u32::from_str_radix(h, 16).ok()?
            } else {
                name.strip_prefix('#')?.parse().ok()?
            };
            char::from_u32(n)?
        }
    };
    Some((c, end + 1))
}

fn hexval(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|v| v as u8)
}

fn quoted_printable(d: &[u8]) -> View {
    let mut v = View::new("quoted-printable", d.len());
    let mut i = 0;
    while i < d.len() {
        if d[i] == b'=' {
            if d.get(i + 1) == Some(&b'\n') {
                i += 2;
                continue;
            }
            if d.get(i + 1) == Some(&b'\r') && d.get(i + 2) == Some(&b'\n') {
                i += 3;
                continue;
            }
            if let (Some(a), Some(b)) = (d.get(i + 1).and_then(|&x| hexval(x)), d.get(i + 2).and_then(|&x| hexval(x))) {
                v.push(a << 4 | b, i);
                i += 3;
                continue;
            }
        }
        v.push(d[i], i);
        i += 1;
    }
    v
}

fn percent_decode(d: &[u8]) -> View {
    let mut v = View::new("url-decoded", d.len());
    let mut i = 0;
    while i < d.len() {
        if d[i] == b'%'
            && let (Some(a), Some(b)) = (d.get(i + 1).and_then(|&x| hexval(x)), d.get(i + 2).and_then(|&x| hexval(x))) {
                v.push(a << 4 | b, i);
                i += 3;
                continue;
            }
        v.push(d[i], i);
        i += 1;
    }
    v
}

fn unescape(d: &[u8]) -> View {
    let mut v = View::new("unescaped", d.len());
    let mut i = 0;
    let hex4 = |s: &[u8]| -> Option<u32> {
        if s.len() < 4 {
            return None;
        }
        u32::from_str_radix(std::str::from_utf8(&s[..4]).ok()?, 16).ok()
    };
    while i < d.len() {
        if d[i] == b'\\' && i + 1 < d.len() {
            match d[i + 1] {
                b'u' => {
                    if let Some(hi) = hex4(&d[i + 2..]) {
                        // surrogate pair?
                        if (0xD800..0xDC00).contains(&hi) && d.get(i + 6) == Some(&b'\\') && d.get(i + 7) == Some(&b'u')
                            && let Some(lo) = hex4(&d[i + 8..])
                                && let Some(c) = char::decode_utf16([hi as u16, lo as u16]).next().and_then(|r| r.ok()) {
                                    v.push_char(c, i);
                                    i += 12;
                                    continue;
                                }
                        if let Some(c) = char::from_u32(hi) {
                            v.push_char(c, i);
                            i += 6;
                            continue;
                        }
                    }
                }
                b'x' => {
                    if let (Some(a), Some(b)) = (d.get(i + 2).and_then(|&x| hexval(x)), d.get(i + 3).and_then(|&x| hexval(x))) {
                        v.push_char(char::from(a << 4 | b), i);
                        i += 4;
                        continue;
                    }
                }
                b'n' | b't' | b'r' => {
                    v.push(b' ', i);
                    i += 2;
                    continue;
                }
                b'/' | b'"' | b'\\' | b'\'' => {
                    v.push(d[i + 1], i);
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        v.push(d[i], i);
        i += 1;
    }
    v
}

/// Pull the string operands of PDF text operators: (Hello) Tj, [(Hel) -20 (lo)] TJ, <48656c6c6f> Tj
fn pdf_text(d: &[u8]) -> View {
    let mut v = View::new("pdf-text", d.len() / 2);
    let mut i = 0;
    let mut depth_arr = 0;
    while i < d.len() {
        match d[i] {
            b'[' => {
                depth_arr += 1;
                i += 1;
            }
            b']' => {
                depth_arr = (depth_arr - 1).max(0);
                i += 1;
            }
            b'(' => {
                let mut level = 1;
                i += 1;
                while i < d.len() && level > 0 {
                    match d[i] {
                        b'\\' if i + 1 < d.len() => {
                            let n = d[i + 1];
                            let c = match n {
                                b'n' | b'r' | b't' => b' ',
                                b'0'..=b'7' => {
                                    let mut val = 0u32;
                                    let mut k = 0;
                                    while k < 3 && i + 1 + k < d.len() && (b'0'..=b'7').contains(&d[i + 1 + k]) {
                                        val = val * 8 + (d[i + 1 + k] - b'0') as u32;
                                        k += 1;
                                    }
                                    v.push_char(char::from_u32(val).unwrap_or('·'), i);
                                    i += 1 + k;
                                    continue;
                                }
                                other => other,
                            };
                            v.push(c, i);
                            i += 2;
                        }
                        b'(' => {
                            level += 1;
                            v.push(b'(', i);
                            i += 1;
                        }
                        b')' => {
                            level -= 1;
                            if level > 0 {
                                v.push(b')', i);
                            }
                            i += 1;
                        }
                        c => {
                            v.push(c, i);
                            i += 1;
                        }
                    }
                }
                if depth_arr == 0 {
                    v.push(b' ', i.saturating_sub(1));
                }
            }
            b'-' | b'0'..=b'9' if depth_arr > 0 => {
                // kerning number inside TJ; large negative values usually mean a space
                let s = i;
                while i < d.len() && (d[i].is_ascii_digit() || d[i] == b'-' || d[i] == b'.') {
                    i += 1;
                }
                if let Ok(n) = std::str::from_utf8(&d[s..i]).unwrap_or("0").parse::<f64>()
                    && n < -180.0 {
                        v.push(b' ', s);
                    }
            }
            b'T' if d.get(i + 1).is_some_and(|c| matches!(c, b'd' | b'D' | b'*' | b'm')) => {
                v.push(b' ', i);
                i += 2;
            }
            _ => i += 1,
        }
    }
    v
}

// ── MIME encoded words (RFC 2047): =?UTF-8?B?…?= / =?ISO-8859-1?Q?…?= ──

fn mime_words(d: &[u8]) -> View {
    let mut v = View::new("mime-header", d.len());
    let mut last = 0;
    let mut after_word = false;
    for c in RE_EW.captures_iter(d) {
        let m = c.get(0).unwrap();
        let between = &d[last..m.start()];
        // whitespace between two adjacent encoded words is not content
        if !(after_word && between.iter().all(|b| b.is_ascii_whitespace())) {
            for (k, &b) in between.iter().enumerate() {
                v.push(b, last + k);
            }
        }
        let charset = String::from_utf8_lossy(&c[1]);
        let enc = encoding_rs::Encoding::for_label(charset.as_bytes()).unwrap_or(encoding_rs::UTF_8);
        let body = &c[3];
        let bytes = if c[2].eq_ignore_ascii_case(b"b") {
            let usable = if body.len() % 4 == 1 { body.len() - 1 } else { body.len() };
            B64.decode(&body[..usable]).unwrap_or_default()
        } else {
            let mut out = Vec::new();
            let mut i = 0;
            while i < body.len() {
                match body[i] {
                    b'_' => out.push(b' '),
                    b'=' if i + 2 < body.len() + 1 => {
                        if let (Some(a), Some(b)) = (body.get(i + 1).and_then(|&x| hexval(x)), body.get(i + 2).and_then(|&x| hexval(x))) {
                            out.push(a << 4 | b);
                            i += 3;
                            continue;
                        }
                        out.push(b'=');
                    }
                    b => out.push(b),
                }
                i += 1;
            }
            out
        };
        let (text, _, _) = enc.decode(&bytes);
        for ch in text.chars() {
            v.push_char(ch, m.start());
        }
        last = m.end();
        after_word = true;
    }
    for (k, &b) in d[last..].iter().enumerate() {
        v.push(b, last + k);
    }
    v
}

// ── RTF ──

const RTF_SKIP: &[&str] = &[
    "fonttbl", "colortbl", "stylesheet", "pict", "themedata", "colorschememapping", "datastore", "latentstyles",
    "listtable", "listoverridetable", "rsidtbl", "generator", "xmlnstbl", "objdata", "fldinst", "filetbl", "revtbl",
];

/// Plain text of an RTF document: control words dropped, `\'hh` decoded with
/// the document code page, `\uN` unicode escapes honoured.
fn rtf_text(d: &[u8]) -> View {
    let cp = d
        .windows(8)
        .position(|w| w == b"\\ansicpg")
        .and_then(|p| std::str::from_utf8(&d[p + 8..(p + 13).min(d.len())]).ok().map(|s| s.chars().take_while(char::is_ascii_digit).collect::<String>()))
        .and_then(|n| encoding_rs::Encoding::for_label(format!("windows-{n}").as_bytes()))
        .unwrap_or(encoding_rs::WINDOWS_1252);
    let mut v = View::new("rtf-text", d.len() / 2);
    let mut stack: Vec<(bool, usize)> = Vec::new();
    let (mut skip, mut uc) = (false, 1usize);
    let mut pending: Vec<u8> = Vec::new(); // consecutive \'hh bytes (multi-byte code pages)
    let mut pending_at = 0;
    let mut i = 0;
    let flush = |v: &mut View, pending: &mut Vec<u8>, at: usize| {
        if !pending.is_empty() {
            let (t, _) = cp.decode_without_bom_handling(pending);
            for c in t.chars() {
                v.push_char(c, at);
            }
            pending.clear();
        }
    };
    while i < d.len() {
        let b = d[i];
        if !(b == b'\\' && d.get(i + 1) == Some(&b'\'')) {
            flush(&mut v, &mut pending, pending_at);
        }
        match b {
            b'{' => {
                stack.push((skip, uc));
                i += 1;
            }
            b'}' => {
                (skip, uc) = stack.pop().unwrap_or((false, 1));
                i += 1;
            }
            b'\r' | b'\n' => i += 1,
            b'\\' => {
                let Some(&n) = d.get(i + 1) else { break };
                if n.is_ascii_alphabetic() {
                    let ws = i + 1;
                    let mut j = ws;
                    while j < d.len() && d[j].is_ascii_alphabetic() {
                        j += 1;
                    }
                    let word = std::str::from_utf8(&d[ws..j]).unwrap_or("");
                    let ps = j;
                    if j < d.len() && (d[j] == b'-' || d[j].is_ascii_digit()) {
                        j += 1;
                        while j < d.len() && d[j].is_ascii_digit() {
                            j += 1;
                        }
                    }
                    let param: Option<i64> = std::str::from_utf8(&d[ps..j]).ok().and_then(|s| s.parse().ok());
                    if j < d.len() && d[j] == b' ' {
                        j += 1;
                    }
                    match word {
                        "par" | "line" | "tab" | "cell" | "row" | "sect" | "page" | "emspace" | "enspace" if !skip => v.push(b' ', i),
                        "u" if !skip => {
                            let n = param.unwrap_or(0);
                            let n = if n < 0 { n + 65536 } else { n } as u32;
                            v.push_char(char::from_u32(n).unwrap_or('·'), i);
                            // skip the ANSI fallback characters
                            for _ in 0..uc {
                                if d.get(j) == Some(&b'\\') && d.get(j + 1) == Some(&b'\'') {
                                    j += 4;
                                } else if j < d.len() && d[j] != b'\\' && d[j] != b'{' && d[j] != b'}' {
                                    j += 1;
                                }
                            }
                        }
                        "uc" => uc = param.unwrap_or(1).max(0) as usize,
                        w if RTF_SKIP.contains(&w) => skip = true,
                        _ => {}
                    }
                    i = j;
                } else {
                    match n {
                        b'\'' => {
                            if let (Some(a), Some(c)) = (d.get(i + 2).and_then(|&x| hexval(x)), d.get(i + 3).and_then(|&x| hexval(x)))
                                && !skip {
                                    if pending.is_empty() {
                                        pending_at = i;
                                    }
                                    pending.push(a << 4 | c);
                                }
                            i += 4;
                        }
                        b'*' => {
                            skip = true;
                            i += 2;
                        }
                        b'~' => {
                            if !skip {
                                v.push(b' ', i);
                            }
                            i += 2;
                        }
                        b'\\' | b'{' | b'}' => {
                            if !skip {
                                v.push(n, i);
                            }
                            i += 2;
                        }
                        _ => i += 2,
                    }
                }
            }
            _ => {
                if !skip {
                    v.push(b, i);
                }
                i += 1;
            }
        }
    }
    flush(&mut v, &mut pending, pending_at);
    v
}

// ── base64 blobs ────────────────────────────────────────────────────────

static B64: LazyLock<GeneralPurpose> = LazyLock::new(|| {
    GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new()
            .with_decode_padding_mode(DecodePaddingMode::Indifferent)
            .with_decode_allow_trailing_bits(true),
    )
});

fn is_b64(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/'
}

/// Find long base64 runs (line breaks / indentation allowed, as in MIME and
/// XML plists) and decode them. Returns (start, end, decoded).
pub fn base64_blobs(d: &[u8], min_chars: usize, max_blobs: usize) -> Vec<(usize, usize, Vec<u8>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < d.len() && out.len() < max_blobs {
        if !is_b64(d[i]) {
            i += 1;
            continue;
        }
        // cheap rejection: a short word not followed by a line break can't start a blob
        let run = d[i..].iter().position(|&c| !is_b64(c)).unwrap_or(d.len() - i);
        if run < 16 || (run < min_chars && !matches!(d.get(i + run), Some(b'\r' | b'\n' | b'='))) {
            i += run.max(1);
            continue;
        }
        let start = i;
        let mut chars = Vec::new();
        let mut j = i;
        let mut all_hex = true;
        while j < d.len() {
            let b = d[j];
            if is_b64(b) {
                all_hex &= b.is_ascii_hexdigit();
                chars.push(b);
                j += 1;
            } else if b == b'\r' || b == b'\n' {
                // a line break continues the blob only if more base64 follows after indentation
                let mut k = j + 1;
                while k < d.len() && matches!(d[k], b' ' | b'\t' | b'\r' | b'\n') {
                    k += 1;
                }
                if k < d.len() && is_b64(d[k]) && chars.len() >= 16 {
                    j = k;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        let mut end = j;
        while end < d.len() && d[end] == b'=' {
            end += 1;
        }
        // must not be glued to other word characters (avoids decoding identifiers)
        let glued = start > 0 && (d[start - 1].is_ascii_alphanumeric() || d[start - 1] == b'_');
        if chars.len() >= min_chars && !all_hex && !glued {
            let usable = if chars.len() % 4 == 1 { chars.len() - 1 } else { chars.len() };
            if let Ok(dec) = B64.decode(&chars[..usable])
                && !dec.is_empty() {
                    out.push((start, end, dec));
                }
        }
        i = end.max(i + 1);
    }
    out
}

/// Decode the base64 run surrounding a match, for display.
pub fn base64_run_text(d: &[u8], s: usize, e: usize) -> Option<String> {
    let mut a = s;
    while a > 0 && is_b64(d[a - 1]) && s - a < 4096 {
        a -= 1;
    }
    let mut b = e;
    while b < d.len() && is_b64(d[b]) && b - e < 4096 {
        b += 1;
    }
    let run = &d[a..b];
    let usable = if run.len() % 4 == 1 { run.len() - 1 } else { run.len() };
    let dec = B64.decode(&run[..usable]).ok()?;
    let txt = crate::hit::clean(&String::from_utf8_lossy(&dec));
    let txt: String = txt.chars().take(120).collect();
    Some(txt)
}

#[cfg(test)]
mod bench {
    use super::*;
    #[test]
    #[ignore]
    #[allow(clippy::type_complexity)]
    fn views_speed() {
        let hay = std::fs::read(std::env::var("BENCH_FILE").unwrap()).unwrap();
        let files: Vec<&[u8]> = hay.chunks(10_000).collect();
        let mbs = |t: std::time::Instant| format!("{:>7.0} MB/s", hay.len() as f64 / t.elapsed().as_secs_f64() / 1e6);
        let checks: [(&str, &LazyLock<Regex>); 6] = [("RE_TAG", &RE_TAG), ("RE_QP", &RE_QP), ("RE_PCT", &RE_PCT), ("RE_ESC", &RE_ESC), ("RE_PDF", &RE_PDF_TEXT), ("RE_EW", &RE_EW)];
        for (n, re) in checks {
            let t = std::time::Instant::now();
            let c = files.iter().filter(|f| re.is_match(f)).count();
            eprintln!("is_match {n:<8} {}  ({c} files)", mbs(t));
        }
        let fs: [(&str, fn(&[u8]) -> View); 4] = [("strip_markup", strip_markup), ("percent", percent_decode), ("unescape", unescape), ("pdf_text", pdf_text)];
        for (n, f) in fs {
            let t = std::time::Instant::now();
            for x in &files { std::hint::black_box(f(x)); }
            eprintln!("build    {n:<12} {}", mbs(t));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_strip_maps_back() {
        let src = b"<w:t>Hel</w:t><w:t>lo &amp; bye</w:t>";
        let v = strip_markup(src);
        let s = String::from_utf8_lossy(&v.data);
        assert!(s.contains("Hello & bye"), "{s}");
        let p = s.find("lo").unwrap();
        assert_eq!(&src[v.map[p] as usize..v.map[p] as usize + 2], b"lo");
    }

    #[test]
    fn blobs() {
        let text = format!("<data>\n\t{}\n\t{}\n</data>", "SGVsbG8gV29ybGQgZnJvbSBhIGxvbmcgYmFzZTY0IGJs", "b2Igc3BsaXQgb3ZlciBsaW5lcw==");
        let b = base64_blobs(text.as_bytes(), 32, 10);
        assert_eq!(b.len(), 1);
        assert_eq!(String::from_utf8_lossy(&b[0].2), "Hello World from a long base64 blob split over lines");
    }

    #[test]
    fn json_escapes() {
        let src = format!("{{\"city\":\"S{}u00e3o Paulo {}ud83d{}ude00\"}}", "\\", "\\", "\\");
        let v = unescape(src.as_bytes());
        assert!(String::from_utf8_lossy(&v.data).contains("São Paulo 😀"));
    }

    #[test]
    fn mime_encoded_words() {
        let v = mime_words(b"Subject: =?UTF-8?B?UGhvZW5peCBQcm90b2NvbA==?= =?ISO-8859-1?Q?_S=E3o_Paulo?=");
        assert_eq!(String::from_utf8_lossy(&v.data), "Subject: Phoenix Protocol São Paulo");
    }

    #[test]
    fn rtf() {
        let src = br"{\rtf1\ansi\ansicpg1252{\fonttbl{\f0 Helvetica;}}\f0 The Phoe\b nix\b0  Prot\'f3col S\u227?o Paulo\par}";
        let v = rtf_text(src);
        assert_eq!(String::from_utf8_lossy(&v.data).trim(), "The Phoenix Protócol São Paulo");
    }

    #[test]
    fn pdf_tj() {
        let v = pdf_text(b"BT /F1 12 Tf [(Hel) -20 (lo) -300 (World)] TJ ET");
        assert!(String::from_utf8_lossy(&v.data).contains("Hello World"));
    }

}
