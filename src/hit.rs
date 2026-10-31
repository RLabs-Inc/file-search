use serde::Serialize;

use crate::matcher::Kind;

/// One match, fully described.
#[derive(Debug, Clone, Serialize)]
pub struct Hit {
    pub id: u64,
    /// The real file on disk.
    pub path: String,
    /// Where inside the file: zip entries, sqlite cells, decoded views… joined by " ⟫ ".
    pub location: String,
    /// How it matched (text, utf-16le, base64, digits, …).
    pub method: String,
    /// Byte offset inside the innermost container.
    pub offset: u64,
    pub line: Option<u64>,
    /// Extra physical detail: sqlite page, WAL frame, …
    pub detail: Option<String>,
    pub before: String,
    pub matched: String,
    pub after: String,
    pub file_size: u64,
    pub modified: Option<String>,
}

impl Hit {
    pub fn context(&self) -> String {
        format!("{}{}{}", self.before, self.matched, self.after)
    }
}

// ── location chain ──────────────────────────────────────────────────────

/// A cheap, lazily-rendered breadcrumb of where we are inside a file.
/// Building one is just a stack push; strings are only produced on a hit.
pub enum Seg<'a> {
    Str(&'a str),
    Owned(String),
    Cell { table: &'a str, rowid: Option<i64>, col: &'a str },
}

pub struct Loc<'a> {
    pub parent: Option<&'a Loc<'a>>,
    pub seg: Option<Seg<'a>>,
}

impl<'a> Loc<'a> {
    pub const ROOT: Loc<'static> = Loc { parent: None, seg: None };

    pub fn child(&'a self, seg: Seg<'a>) -> Loc<'a> {
        Loc { parent: Some(self), seg: Some(seg) }
    }

    pub fn render(&self) -> String {
        let mut parts = Vec::new();
        let mut cur = Some(self);
        while let Some(l) = cur {
            if let Some(s) = &l.seg {
                parts.push(match s {
                    Seg::Str(s) => s.to_string(),
                    Seg::Owned(s) => s.clone(),
                    Seg::Cell { table, rowid, col } => match rowid {
                        Some(r) => format!("sqlite:{table}[rowid {r}].{col}"),
                        None => format!("sqlite:{table}.{col}"),
                    },
                });
            }
            cur = l.parent;
        }
        parts.reverse();
        parts.join(" ⟫ ")
    }
}

// ── context rendering ───────────────────────────────────────────────────

fn clean_char(c: char) -> char {
    match c {
        '\n' => '⏎',
        '\r' => ' ',
        '\t' => ' ',
        '\u{FFFD}' => '·',
        c if c.is_control() => '·',
        c if ('\u{200B}'..='\u{200F}').contains(&c) || c == '\u{FEFF}' => '·',
        c => c,
    }
}

/// Clean a decoded string for display: control chars → '·', long '·' runs squashed.
pub fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dots = 0;
    for c in s.chars().map(clean_char) {
        if c == '·' {
            dots += 1;
            if dots > 3 {
                continue;
            }
        } else {
            dots = 0;
        }
        out.push(c);
    }
    out
}

fn decode_utf16(bytes: &[u8], be: bool) -> String {
    let units = bytes.chunks_exact(2).map(|c| {
        if be { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }
    });
    char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')).collect()
}

fn last_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    s.chars().skip(count.saturating_sub(n)).collect()
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn readable(b: &[u8]) -> bool {
    std::str::from_utf8(b).is_ok_and(|s| s.chars().all(|c| !c.is_control() || c == '\n' || c == '\t'))
}

/// Returns (before, matched, after) for a match, decoded according to how it matched.
pub fn render_context(data: &[u8], start: usize, end: usize, kind: Kind, ctx: usize) -> (String, String, String) {
    match kind {
        Kind::Utf16Le | Kind::Utf16Be => {
            let be = kind == Kind::Utf16Be;
            let b0 = start - (start.min(ctx * 2) & !1);
            let e1 = (end + ctx * 2).min(data.len());
            let before = decode_utf16(&data[b0..start], be);
            let matched = decode_utf16(&data[start..end], be);
            let after = decode_utf16(&data[end..e1 - ((e1 - end) % 2)], be);
            (clean(&last_chars(&before, ctx)), clean(&matched), clean(&first_chars(&after, ctx)))
        }
        Kind::Legacy(enc) => {
            let b0 = start.saturating_sub(ctx * 2);
            let e1 = (end + ctx * 2).min(data.len());
            let dec = |b: &[u8]| enc.decode_without_bom_handling(b).0.to_string();
            (clean(&last_chars(&dec(&data[b0..start]), ctx)), clean(&dec(&data[start..end])), clean(&first_chars(&dec(&data[end..e1]), ctx)))
        }
        Kind::HexBytes if !readable(&data[start..end]) => {
            let b0 = start.saturating_sub(ctx / 3);
            let e1 = (end + ctx / 3).min(data.len());
            let hx = |s: &[u8]| s.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ");
            let pad = |s: String| if s.is_empty() { s } else { s + " " };
            (pad(hx(&data[b0..start])), hx(&data[start..end]), {
                let a = hx(&data[end..e1]);
                if a.is_empty() { a } else { format!(" {a}") }
            })
        }
        _ => {
            let b0 = start.saturating_sub(ctx * 4);
            let e1 = (end + ctx * 4).min(data.len());
            let before = String::from_utf8_lossy(&data[b0..start]);
            let matched = String::from_utf8_lossy(&data[start..end]);
            let after = String::from_utf8_lossy(&data[end..e1]);
            (clean(&last_chars(&before, ctx)), clean(&matched), clean(&first_chars(&after, ctx)))
        }
    }
}
