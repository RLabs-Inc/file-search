//! Terminal presentation: header, live status zone, hit lines, final summary.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Mutex;
use std::time::Instant;

use prism::{format_bytes, format_number, hex, s, ActiveFrame};

use crate::hit::Hit;
use crate::scan::{Slot, Stats};

pub const BLUE: u32 = 0x7AA2F7;
pub const PURPLE: u32 = 0xBB9AF7;
pub const CYAN: u32 = 0x7DCFFF;
pub const GREEN: u32 = 0x9ECE6A;
pub const ORANGE: u32 = 0xFF9E64;
pub const RED: u32 = 0xF7768E;
pub const GOLD: u32 = 0xE0AF68;
pub const INK: u32 = 0x1A1B26;

/// Terminal width, never 0 (some ptys report 0×0).
pub fn term_width() -> u16 {
    match prism::term_width() {
        0 => 100,
        w => w,
    }
}

/// Interactive terminal we can draw a live zone on.
pub fn live_tty() -> bool {
    prism::is_tty() && prism::term_width() > 0
}

pub fn c(color: u32, t: &str) -> String {
    s().fg(hex(color)).paint(t)
}
pub fn cb(color: u32, t: &str) -> String {
    s().fg(hex(color)).bold().paint(t)
}
pub fn dim(t: &str) -> String {
    s().dim().paint(t)
}
pub fn bold(t: &str) -> String {
    s().bold().paint(t)
}
fn mark(t: &str) -> String {
    s().bold().fg(hex(INK)).bg_color(hex(GOLD)).paint(t)
}

fn lerp(a: u32, b: u32, t: f64) -> u32 {
    let ch = |x: u32, sh: u32| ((x >> sh) & 0xFF) as f64;
    let mix = |sh: u32| ((ch(a, sh) + (ch(b, sh) - ch(a, sh)) * t).round() as u32) << sh;
    mix(16) | mix(8) | mix(0)
}

pub fn gradient(t: &str) -> String {
    let n = t.chars().count().max(2) as f64 - 1.0;
    t.chars().enumerate().map(|(i, ch)| cb(lerp(BLUE, PURPLE, i as f64 / n), &ch.to_string())).collect()
}

pub fn home_short(p: &str) -> String {
    if let Ok(h) = std::env::var("HOME")
        && !h.is_empty() && p.starts_with(&h) {
            return format!("~{}", &p[h.len()..]);
        }
    p.to_string()
}

/// Truncate in the middle, keeping the start and (more of) the end of a path.
pub fn mid_trunc(t: &str, max: usize) -> String {
    let n = t.chars().count();
    if n <= max || max < 8 {
        return t.to_string();
    }
    let head = (max - 1) / 3;
    let tail = max - 1 - head;
    let h: String = t.chars().take(head).collect();
    let tl: String = t.chars().skip(n - tail).collect();
    format!("{h}…{tl}")
}

fn fmt_dur(secs: f64) -> String {
    let s = secs as u64;
    if s >= 3600 {
        format!("{}h{:02}m{:02}s", s / 3600, (s % 3600) / 60, s % 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{secs:.1}s")
    }
}

pub fn duration(secs: f64) -> String {
    fmt_dur(secs)
}

fn styled_path(p: &str) -> String {
    let p = home_short(p);
    match p.rfind('/') {
        Some(i) => format!("{}{}", dim(&p[..=i]), bold(&p[i + 1..])),
        None => bold(&p),
    }
}

// ── header ──────────────────────────────────────────────────────────────

pub fn header(needle: &str, roots: &[String], variants: &[String], out: Option<&str>, threads: usize) -> String {
    let w = term_width() as usize;
    let mut o = String::new();
    o.push('\n');
    o.push_str(&format!("  {} {}\n", gradient("◆ file-search"), dim("· every byte, every encoding")));
    o.push_str(&format!("  {}\n", dim(&"─".repeat(w.saturating_sub(4).min(76)))));
    let shown: String = needle.split_whitespace().collect::<Vec<_>>().join(" ");
    o.push_str(&format!("  {}  {}\n", dim("find   "), cb(GOLD, &format!("“{}”", mid_trunc(&shown, w.saturating_sub(16))))));
    for (i, r) in roots.iter().enumerate() {
        o.push_str(&format!("  {}  {}\n", dim(if i == 0 { "in     " } else { "       " }), c(BLUE, &home_short(r))));
    }
    let vars = variants.iter().map(|v| c(PURPLE, v)).collect::<Vec<_>>().join(&dim(" · "));
    o.push_str(&format!("  {}  {}\n", dim("as     "), vars));
    match out {
        Some(d) => o.push_str(&format!("  {}  {}\n", dim("export "), c(CYAN, &home_short(d)))),
        None => o.push_str(&format!("  {}  {}\n", dim("export "), dim("disabled"))),
    }
    o.push_str(&format!("  {}  {}\n", dim("workers"), dim(&threads.to_string())));
    o.push_str(&format!("  {}\n", dim(&"─".repeat(w.saturating_sub(4).min(76)))));
    o
}

// ── live status zone ────────────────────────────────────────────────────

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn bar(frac: Option<f64>, width: usize, tick: usize) -> String {
    let mut out = String::new();
    match frac {
        Some(f) => {
            let filled = ((f.clamp(0.0, 1.0)) * width as f64).round() as usize;
            for i in 0..width {
                if i < filled {
                    out.push_str(&c(lerp(BLUE, PURPLE, i as f64 / width as f64), "━"));
                } else {
                    out.push_str(&dim("─"));
                }
            }
        }
        None => {
            // indeterminate: a gliding comet while we still discover files
            let pos = tick % (width + 8);
            for i in 0..width {
                let d = pos as isize - i as isize;
                if (0..6).contains(&d) {
                    out.push_str(&c(lerp(PURPLE, BLUE, d as f64 / 6.0), "━"));
                } else {
                    out.push_str(&dim("─"));
                }
            }
        }
    }
    out
}

pub fn live_frame(stats: &Stats, slots: &[Mutex<Slot>], started: Instant, tick: usize) -> ActiveFrame {
    let w = term_width() as usize;
    let el = started.elapsed().as_secs_f64();
    let found = stats.files_found.load(Relaxed);
    let bfound = stats.bytes_found.load(Relaxed);
    let done = stats.files_done.load(Relaxed);
    let bdone = stats.bytes_done.load(Relaxed);
    let walking = !stats.walk_done.load(Relaxed);
    let hits = stats.hits.load(Relaxed);
    let fh = stats.files_hit.load(Relaxed);
    let errs = stats.errors.load(Relaxed);
    let cancel = stats.cancel.load(Relaxed);

    let frac = if walking || bfound == 0 {
        None
    } else {
        Some(bdone as f64 / bfound as f64)
    };
    let spin = SPIN[tick % SPIN.len()];
    let state = if cancel {
        cb(ORANGE, "stopping")
    } else if walking {
        cb(BLUE, "searching")
    } else {
        cb(PURPLE, "searching")
    };
    let pct = match frac {
        Some(f) => cb(PURPLE, &format!("{:>3.0}%", f * 100.0)),
        None => dim("···"),
    };
    let rate = if el > 0.5 { format!("{}/s", format_bytes((bdone as f64 / el) as u64)) } else { "–".into() };
    let bar_w = if w >= 110 { 30 } else if w >= 90 { 22 } else { 14 };
    let line1 = format!(
        "  {} {}  {} {}  {}  {}  {}",
        c(PURPLE, spin),
        state,
        bar(frac, bar_w, tick),
        pct,
        dim(&format!("{} / {}{}", format_bytes(bdone), format_bytes(bfound), if walking { "+" } else { "" })),
        dim(&rate),
        dim(&fmt_dur(el)),
    );
    let hit_s = if hits > 0 { cb(GOLD, &format_number(hits)) } else { dim("0") };
    let err_s = if errs > 0 { c(ORANGE, &format_number(errs)) } else { dim("0") };
    let line2 = format!(
        "  {} {} {}   {} {} {}   {} {}   {} {}",
        dim("files"),
        bold(&format_number(done)),
        dim(&format!("/ {}{}", format_number(found), if walking { " found so far" } else { "" })),
        dim("hits"),
        hit_s,
        dim(&format!("in {} files", format_number(fh))),
        dim("unreadable"),
        err_s,
        dim("folders"),
        dim(&format_number(stats.dirs.load(Relaxed))),
    );
    let mut lines = vec![String::new(), line1, line2];

    let busy: Vec<(String, String, u64, f64)> = slots
        .iter()
        .filter_map(|m| m.try_lock().ok())
        .filter(|s| s.busy)
        .map(|s| (s.path.clone(), s.inner.clone(), s.size, s.since.elapsed().as_secs_f64()))
        .collect();
    let max_rows = 6usize;
    let n = busy.len();
    let mut sorted = busy;
    sorted.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    for (i, (path, inner, size, age)) in sorted.iter().take(max_rows).enumerate() {
        let last = i + 1 == n.min(max_rows) && n <= max_rows;
        let branch = if last { "└" } else { "├" };
        let meta = format!(" {}{}", format_bytes(*size), if *age >= 2.0 { format!(" · {}", fmt_dur(*age)) } else { String::new() });
        let inner_s = if inner.is_empty() { String::new() } else { format!(" ⟫ {inner}") };
        let room = w.saturating_sub(8 + meta.chars().count());
        let full = format!("{}{}", home_short(path), inner_s);
        let shown = mid_trunc(&full, room);
        // color the inner part differently
        let (p, inn) = match shown.find(" ⟫ ") {
            Some(k) => (shown[..k].to_string(), shown[k..].to_string()),
            None => (shown.clone(), String::new()),
        };
        lines.push(format!("  {} {}{}{}", dim(branch), c(CYAN, &p), c(PURPLE, &inn), dim(&meta)));
    }
    if n > max_rows {
        lines.push(format!("  {} {}", dim("└"), dim(&format!("+{} more workers busy", n - max_rows))));
    }
    if cancel {
        lines.push(format!("  {}", c(ORANGE, "Ctrl-C received · finishing current files and writing exports (press again to abort)")));
    }
    ActiveFrame { lines, cursor: None }
}

// ── hit lines ───────────────────────────────────────────────────────────

pub fn file_header(h: &Hit) -> String {
    let mut meta = Vec::new();
    if h.file_size > 0 {
        meta.push(format_bytes(h.file_size));
    }
    if let Some(m) = &h.modified {
        meta.push(m.clone());
    }
    format!("\n  {} {}  {}", c(GOLD, "◆"), styled_path(&h.path), dim(&meta.join(" · ")))
}

pub fn hit_lines(h: &Hit) -> String {
    let w = (term_width() as usize).max(40);
    let mut top = format!("    {} {}", c(GOLD, "▸"), c(PURPLE, &h.method));
    if !h.location.is_empty() {
        top.push_str(&format!(" {}", c(BLUE, &mid_trunc(&format!("⟫ {}", h.location), w.saturating_sub(30)))));
    }
    if let Some(l) = h.line {
        top.push_str(&dim(&format!("  line {l}")));
    } else if !h.method.contains("name") {
        top.push_str(&dim(&format!("  @0x{:x}", h.offset)));
    }
    if let Some(d) = &h.detail {
        top.push_str(&format!("  {}", c(ORANGE, d)));
    }
    top.push_str(&dim(&format!("  #{}", h.id)));

    // center the match in the available width
    let room = w.saturating_sub(10);
    let mlen = h.matched.chars().count();
    let matched: String = if mlen > room.saturating_sub(4) {
        let keep = room.saturating_sub(5);
        format!("{}…", h.matched.chars().take(keep).collect::<String>())
    } else {
        h.matched.clone()
    };
    let rest = room.saturating_sub(matched.chars().count() + 2);
    let blen = h.before.chars().count();
    let alen = h.after.chars().count();
    let mut bshow = (rest / 2).min(blen);
    let ashow = (rest - bshow).min(alen);
    bshow = (rest - ashow).min(blen);
    let before: String = h.before.chars().skip(blen - bshow).collect();
    let after: String = h.after.chars().take(ashow).collect();
    let ctx = format!("      {}{}{}{}{}", dim("…"), before, mark(&matched), after, dim("…"));
    format!("{top}\n{ctx}")
}

pub fn more_line(path: &str, limit: usize) -> String {
    format!("    {} {}", dim("⋯"), dim(&format!("more hits in {} (screen shows {limit}, every hit is exported)", home_short(path))))
}

pub fn capped_line(path: &str, limit: usize) -> String {
    format!("    {} {}", c(ORANGE, "⚠"), c(ORANGE, &format!("{} reached --max-hits {limit}; stopped recording this file", home_short(path))))
}
