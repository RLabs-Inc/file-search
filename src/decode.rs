//! Format detection, decompressors and "derived views" of text.
//!
//! A derived view is a transformed copy of a buffer (markup stripped, URL
//! decoded, …) plus a map from every output byte back to the original offset,
//! so hits can be de-duplicated against the raw scan and reported precisely.

use std::io::{Cursor, Read};
use std::sync::LazyLock;

use base64::Engine;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use chrono::{DateTime, Local, TimeZone, Utc};
use regex::bytes::Regex;

// ── format detection ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Gzip,
    Bzip2,
    Xz,
    Lzma,
    Zstd,
    Zlib,
    Zip,
    Tar,
    BPlist,
    Sqlite,
    SqliteWal,
    Pdf,
    Other,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Format::Gzip => "gzip",
            Format::Bzip2 => "bzip2",
            Format::Xz => "xz",
            Format::Lzma => "lzma",
            Format::Zstd => "zstd",
            Format::Zlib => "zlib",
            Format::Zip => "zip",
            Format::Tar => "tar",
            Format::BPlist => "bplist",
            Format::Sqlite => "sqlite",
            Format::SqliteWal => "sqlite-wal",
            Format::Pdf => "pdf",
            Format::Other => "",
        }
    }
}

pub fn detect(d: &[u8]) -> Format {
    let starts = |m: &[u8]| d.starts_with(m);
    if starts(&[0x1f, 0x8b]) {
        Format::Gzip
    } else if starts(b"BZh") && d.len() > 4 && d[3].is_ascii_digit() {
        Format::Bzip2
    } else if starts(&[0xFD, b'7', b'z', b'X', b'Z', 0]) {
        Format::Xz
    } else if starts(&[0x28, 0xB5, 0x2F, 0xFD]) {
        Format::Zstd
    } else if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") {
        Format::Zip
    } else if starts(b"bplist0") {
        Format::BPlist
    } else if starts(b"SQLite format 3\0") {
        Format::Sqlite
    } else if d.len() >= 32 && (starts(&[0x37, 0x7f, 0x06, 0x82]) || starts(&[0x37, 0x7f, 0x06, 0x83])) {
        Format::SqliteWal
    } else if starts(b"%PDF") {
        Format::Pdf
    } else if d.len() > 262 && &d[257..262] == b"ustar" {
        Format::Tar
    } else if d.len() > 13 && d[0] == 0x5D && d[1] == 0 && d[2] == 0 && (d[5..13] == [0xFF; 8] || d[12] == 0) {
        Format::Lzma
    } else if d.len() > 2 && d[0] == 0x78 && matches!(d[1], 0x01 | 0x5E | 0x9C | 0xDA) {
        Format::Zlib
    } else {
        Format::Other
    }
}

// ── decompression ───────────────────────────────────────────────────────

/// Read up to `cap` bytes; a truncated/corrupt stream still yields what decoded.
pub fn read_capped<R: Read>(mut r: R, cap: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    while out.len() < cap {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n.min(cap - out.len())]),
            Err(_) => break,
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

pub fn decompress(fmt: Format, d: &[u8], cap: usize) -> Option<Vec<u8>> {
    match fmt {
        Format::Gzip => read_capped(flate2::read::MultiGzDecoder::new(d), cap),
        Format::Zlib => read_capped(flate2::read::ZlibDecoder::new(d), cap),
        Format::Bzip2 => read_capped(bzip2::read::MultiBzDecoder::new(d), cap),
        Format::Zstd => {
            let dec = ruzstd::decoding::StreamingDecoder::new(Cursor::new(d)).ok()?;
            read_capped(dec, cap)
        }
        Format::Xz | Format::Lzma => {
            // lzma-rs writes into a sink; cap it with a limited writer
            let mut w = CapWriter { buf: Vec::new(), cap };
            let mut input = std::io::BufReader::new(d);
            let _ = if fmt == Format::Xz {
                lzma_rs::xz_decompress(&mut input, &mut w)
            } else {
                lzma_rs::lzma_decompress(&mut input, &mut w)
            };
            if w.buf.is_empty() { None } else { Some(w.buf) }
        }
        _ => None,
    }
}

struct CapWriter {
    buf: Vec<u8>,
    cap: usize,
}

impl std::io::Write for CapWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() >= self.cap {
            return Err(std::io::Error::other("cap reached"));
        }
        let n = data.len().min(self.cap - self.buf.len());
        self.buf.extend_from_slice(&data[..n]);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

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
        let b = d[i];
        if b == b'<'
            && let Some(m) = RE_TAG.find_at(d, i).filter(|m| m.start() == i) {
                let tag = &d[m.start() + 1..m.end() - 1];
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
                i = m.end();
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

// ── PDF streams ─────────────────────────────────────────────────────────

/// Inflate every FlateDecode stream in a PDF. Returns (offset, decoded).
pub fn pdf_streams(d: &[u8], cap: usize, max: usize) -> Vec<(usize, Vec<u8>)> {
    static RE_STREAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u)stream\r?\n").unwrap());
    let mut out = Vec::new();
    let mut total = 0;
    for m in RE_STREAM.find_iter(d) {
        if out.len() >= max || total >= cap {
            break;
        }
        // look back for the stream dictionary
        let dict_start = m.start().saturating_sub(512);
        let dict = &d[dict_start..m.start()];
        if !dict.windows(11).any(|w| w == b"FlateDecode") {
            continue;
        }
        let body = &d[m.end()..];
        let body = match body.windows(9).position(|w| w == b"endstream") {
            Some(p) => &body[..p],
            None => body,
        };
        if let Some(dec) = read_capped(flate2::read::ZlibDecoder::new(body), cap - total) {
            total += dec.len();
            out.push((m.end(), dec));
        }
    }
    out
}

// ── timestamps ──────────────────────────────────────────────────────────

const UNIX_2000: f64 = 946_684_800.0;
const UNIX_2040: f64 = 2_208_988_800.0;
const MAC_EPOCH: f64 = 978_307_200.0; // 2001-01-01
const WEBKIT_EPOCH: f64 = 11_644_473_600.0; // 1601-01-01 → 1970

fn fmt_unix(secs: f64, label: &str) -> String {
    let utc: DateTime<Utc> = Utc
        .timestamp_opt(secs.floor() as i64, 0)
        .single()
        .unwrap_or_default();
    let local = utc.with_timezone(&Local);
    format!(
        "⟨{label} {} UTC · {} local⟩",
        utc.format("%Y-%m-%d %H:%M:%S"),
        local.format("%Y-%m-%d %H:%M:%S")
    )
}

/// Human-readable interpretations of a number that looks like a timestamp.
/// `hinted` = the column/key name suggests a date, which unlocks the riskier epochs.
pub fn timestamp_notes(v: f64, hinted: bool) -> Option<String> {
    let mut notes = Vec::new();
    let in_range = |s: f64| (UNIX_2000..UNIX_2040).contains(&s);
    if in_range(v) {
        notes.push(fmt_unix(v, "unix"));
    } else if in_range(v / 1e3) {
        notes.push(fmt_unix(v / 1e3, "unix-ms"));
    } else if in_range(v / 1e6) && hinted {
        notes.push(fmt_unix(v / 1e6, "unix-µs"));
    }
    if hinted || v.fract() != 0.0 {
        if in_range(v + MAC_EPOCH) && v > 1e8 {
            notes.push(fmt_unix(v + MAC_EPOCH, "mac"));
        } else if in_range(v / 1e9 + MAC_EPOCH) && v > 1e16 {
            notes.push(fmt_unix(v / 1e9 + MAC_EPOCH, "mac-ns"));
        }
    }
    if in_range(v / 1e6 - WEBKIT_EPOCH) && v > 1e16 {
        notes.push(fmt_unix(v / 1e6 - WEBKIT_EPOCH, "webkit"));
    }
    if notes.is_empty() { None } else { Some(notes.join(" ")) }
}

pub fn name_hints_time(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    ["date", "time", "stamp", "created", "modified", "updated", "visit", "expire", "_at", "last", "when", "start", "end"]
        .iter()
        .any(|k| n.contains(k))
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
    fn pdf_tj() {
        let v = pdf_text(b"BT /F1 12 Tf [(Hel) -20 (lo) -300 (World)] TJ ET");
        assert!(String::from_utf8_lossy(&v.data).contains("Hello World"));
    }

    #[test]
    fn timestamps() {
        assert!(timestamp_notes(1_710_460_800.0, false).unwrap().contains("2024-03-15"));
        assert!(timestamp_notes(732_153_600.0, true).unwrap().contains("mac 2024-03-15"));
        assert!(timestamp_notes(13_354_934_400_000_000.0, false).unwrap().contains("webkit 2024-03-15"));
    }
}
