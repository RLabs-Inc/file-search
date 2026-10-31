//! The scanning engine: walks folders, reads files, recursively decodes
//! containers and reports every hit through a channel.

use std::cell::RefCell;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::{Instant, SystemTime};

use crossbeam_channel::Sender;

use crate::formats::{self, archive, compress, text, time, Format};
use crate::hit::{clean, render_context, Hit, Loc, Seg};
use crate::matcher::{Kind, Matcher};

pub struct Config {
    pub roots: Vec<PathBuf>,
    pub context: usize,
    pub decode: bool,
    pub max_depth: usize,
    pub max_decode: u64,
    pub decomp_cap: usize,
    pub max_hits_per_file: usize,
    pub follow_links: bool,
    pub one_fs: bool,
    pub xattrs: bool,
    pub names: bool,
    pub excludes: Vec<String>,
    pub out_dir: Option<PathBuf>,
    pub threads: usize,
}

pub enum Event {
    Hit(Box<Hit>),
    Error { path: String, msg: String },
    Capped { path: String, limit: usize },
}

#[derive(Default)]
pub struct Stats {
    pub files_found: AtomicU64,
    pub bytes_found: AtomicU64,
    pub dirs: AtomicU64,
    pub walk_done: AtomicBool,
    pub files_done: AtomicU64,
    pub bytes_done: AtomicU64,
    pub hits: AtomicU64,
    pub files_hit: AtomicU64,
    pub errors: AtomicU64,
    pub cancel: AtomicBool,
}

pub struct Slot {
    pub path: String,
    pub inner: String,
    pub size: u64,
    pub since: Instant,
    pub busy: bool,
}

impl Default for Slot {
    fn default() -> Self {
        Slot { path: String::new(), inner: String::new(), size: 0, since: Instant::now(), busy: false }
    }
}

// ── panic capture (decoders must never take the whole run down) ─────────

thread_local! {
    static LAST_PANIC: RefCell<String> = const { RefCell::new(String::new()) };
    static IN_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Panics inside workers are captured and reported as file errors;
/// anywhere else they keep Rust's normal behaviour.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if IN_WORKER.with(|w| w.get()) {
            let msg = info.to_string();
            LAST_PANIC.with(|p| *p.borrow_mut() = msg);
        } else {
            crate::restore_terminal();
            default(info);
        }
    }));
}

fn take_panic() -> String {
    LAST_PANIC.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

// ── walker ──────────────────────────────────────────────────────────────

fn fmt_mtime(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn excluded(path: &Path, cfg: &Config) -> bool {
    if let Some(out) = &cfg.out_dir
        && path.starts_with(out) {
            return true;
        }
    if cfg!(target_os = "linux") && (path.starts_with("/proc") || path.starts_with("/sys")) {
        return true;
    }
    if cfg.excludes.is_empty() {
        return false;
    }
    let s = path.to_string_lossy();
    cfg.excludes.iter().any(|ex| {
        if ex.contains('/') {
            s.contains(ex.as_str())
        } else {
            path.file_name().is_some_and(|n| n.to_string_lossy() == *ex)
        }
    })
}

/// Name hits as standalone `Hit`s (used for directory and file names).
fn name_hits(m: &Matcher, name: &str, full: &str, method: &str, ctx: usize) -> Vec<Hit> {
    let mut out = Vec::new();
    let base = full.len() - name.len();
    m.for_each(name.as_bytes(), |r| {
        let (b, mm, a) = render_context(full.as_bytes(), base + r.start, base + r.end, r.kind, ctx.max(80));
        out.push(Hit {
            id: 0,
            path: full.to_string(),
            location: String::new(),
            method: format!("{method} · {}", r.kind.label()),
            offset: 0,
            line: None,
            detail: None,
            before: b,
            matched: mm,
            after: a,
            file_size: 0,
            modified: None,
        });
        true
    });
    out
}

pub fn walk(cfg: &Config, m: &Matcher, stats: &Stats, jobs: Sender<(PathBuf, u64)>, ev: Sender<Event>) {
    for root in &cfg.roots {
        let it = walkdir::WalkDir::new(root)
            .follow_links(cfg.follow_links)
            .same_file_system(cfg.one_fs)
            .into_iter()
            .filter_entry(|e| !excluded(e.path(), cfg));
        for entry in it {
            if stats.cancel.load(Relaxed) {
                break;
            }
            let e = match entry {
                Ok(e) => e,
                Err(err) => {
                    stats.errors.fetch_add(1, Relaxed);
                    let path = err.path().map(|p| p.display().to_string()).unwrap_or_default();
                    let msg = err.io_error().map(|e| e.to_string()).unwrap_or_else(|| err.to_string());
                    let _ = ev.send(Event::Error { path, msg });
                    continue;
                }
            };
            let ft = e.file_type();
            if ft.is_dir() {
                stats.dirs.fetch_add(1, Relaxed);
                if cfg.names && e.depth() > 0 {
                    let name = e.file_name().to_string_lossy();
                    if m.is_match(name.as_bytes()) {
                        for mut h in name_hits(m, &name, &e.path().to_string_lossy(), "folder name", cfg.context) {
                            if let Ok(md) = e.metadata() {
                                h.modified = md.modified().ok().map(fmt_mtime);
                            }
                            stats.hits.fetch_add(1, Relaxed);
                            stats.files_hit.fetch_add(1, Relaxed);
                            let _ = ev.send(Event::Hit(Box::new(h)));
                        }
                    }
                }
            } else if ft.is_file() {
                let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                stats.files_found.fetch_add(1, Relaxed);
                stats.bytes_found.fetch_add(size, Relaxed);
                if jobs.send((e.into_path(), size)).is_err() {
                    break;
                }
            }
            // sockets, fifos, devices and unfollowed symlinks are never opened
        }
    }
    stats.walk_done.store(true, Relaxed);
}

// ── per-file scanner ────────────────────────────────────────────────────

pub struct Worker<'a> {
    pub m: &'a Matcher,
    pub cfg: &'a Config,
    pub stats: &'a Stats,
    pub slot: &'a Mutex<Slot>,
    pub ev: Sender<Event>,
}

impl Worker<'_> {
    pub fn run(&self, path: PathBuf, size: u64) {
        {
            let mut s = self.slot.lock().unwrap();
            s.path = path.display().to_string();
            s.inner.clear();
            s.size = size;
            s.since = Instant::now();
            s.busy = true;
        }
        IN_WORKER.with(|w| w.set(true));
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut fs = FileScan::new(self, &path, size);
            fs.scan_file(&path);
        }));
        IN_WORKER.with(|w| w.set(false));
        if res.is_err() {
            self.error(&path.display().to_string(), format!("decoder crashed: {}", take_panic()));
        }
        self.stats.files_done.fetch_add(1, Relaxed);
        self.stats.bytes_done.fetch_add(size, Relaxed);
        self.slot.lock().unwrap().busy = false;
    }

    fn error(&self, path: &str, msg: String) {
        self.stats.errors.fetch_add(1, Relaxed);
        let _ = self.ev.send(Event::Error { path: path.to_string(), msg });
    }
}

enum Data {
    Vec(Vec<u8>),
    Map(memmap2::Mmap),
}

impl std::ops::Deref for Data {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            Data::Vec(v) => v,
            Data::Map(m) => m,
        }
    }
}

struct FileScan<'a, 'w> {
    w: &'w Worker<'a>,
    path: String,
    size: u64,
    modified: Option<String>,
    hits: usize,
    capped: bool,
    any_hit: bool,
}

const READ_LIMIT: u64 = 32 << 20; // read small files, mmap big ones
const VIEW_LIMIT: usize = 64 << 20; // derived text views only for buffers up to this size

impl<'a, 'w> FileScan<'a, 'w> {
    fn new(w: &'w Worker<'a>, path: &Path, size: u64) -> Self {
        let modified = std::fs::symlink_metadata(path).ok().and_then(|m| m.modified().ok()).map(fmt_mtime);
        FileScan { w, path: path.display().to_string(), size, modified, hits: 0, capped: false, any_hit: false }
    }

    fn stop(&self) -> bool {
        self.capped || self.w.stats.cancel.load(Relaxed)
    }

    fn set_inner(&self, loc: &Loc) {
        if let Ok(mut s) = self.w.slot.try_lock() {
            s.inner = loc.render();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit(&mut self, loc: &Loc, method: String, offset: usize, line: Option<u64>, detail: Option<String>, ctx: (String, String, String)) -> bool {
        if self.hits >= self.w.cfg.max_hits_per_file {
            if !self.capped {
                self.capped = true;
                let _ = self.w.ev.send(Event::Capped { path: self.path.clone(), limit: self.w.cfg.max_hits_per_file });
            }
            return false;
        }
        self.hits += 1;
        self.w.stats.hits.fetch_add(1, Relaxed);
        if !self.any_hit {
            self.any_hit = true;
            self.w.stats.files_hit.fetch_add(1, Relaxed);
        }
        let (before, matched, after) = ctx;
        let _ = self.w.ev.send(Event::Hit(Box::new(Hit {
            id: 0,
            path: self.path.clone(),
            location: loc.render(),
            method,
            offset: offset as u64,
            line,
            detail,
            before,
            matched,
            after,
            file_size: self.size,
            modified: self.modified.clone(),
        })));
        true
    }

    /// Values we rendered ourselves (numbers + date notes): one hit per value is enough.
    fn scan_first(&mut self, text: &[u8], loc: &Loc) {
        let mut first = None;
        self.w.m.for_each(text, |r| {
            first = Some(r);
            false
        });
        if let Some(r) = first {
            let ctx = render_context(text, r.start, r.end, r.kind, self.w.cfg.context.max(60));
            self.emit(loc, r.kind.label().to_string(), r.start, None, None, ctx);
        }
    }

    /// Match a short name (archive entry, xattr name, plist key…).
    fn check_name(&mut self, name: &str, loc: &Loc, method: &str) {
        if !self.w.m.is_match(name.as_bytes()) {
            return;
        }
        let mut found = Vec::new();
        self.w.m.for_each(name.as_bytes(), |r| {
            found.push(r);
            true
        });
        for r in found {
            let ctx = render_context(name.as_bytes(), r.start, r.end, r.kind, self.w.cfg.context.max(60));
            if !self.emit(loc, format!("{method} · {}", r.kind.label()), r.start, None, None, ctx) {
                return;
            }
        }
    }

    fn scan_file(&mut self, path: &Path) {
        let cfg = self.w.cfg;
        if cfg.names
            && let Some(name) = path.file_name() {
                let name = name.to_string_lossy();
                if self.w.m.is_match(name.as_bytes()) {
                    let full = self.path.clone();
                    for h in name_hits(self.w.m, &name, &full, "file name", cfg.context) {
                        let ctx = (h.before, h.matched, h.after);
                        if !self.emit(&Loc::ROOT, h.method, 0, None, None, ctx) {
                            break;
                        }
                    }
                }
            }

        if self.size > 0 {
            match load(path, self.size) {
                Ok(data) => self.scan_buffer(&data, &Loc::ROOT, 0, Some(path)),
                Err(e) => self.w.error(&self.path, e.to_string()),
            }
        }

        if cfg.xattrs && !self.stop()
            && let Ok(names) = xattr::list(path) {
                for name in names {
                    let name = name.to_string_lossy().to_string();
                    let label = format!("xattr:{name}");
                    let loc = Loc::ROOT.child(Seg::Str(&label));
                    self.check_name(&name, &Loc::ROOT, "xattr name");
                    if let Ok(Some(val)) = xattr::get(path, &name) {
                        self.scan_buffer(&val, &loc, 1, None);
                    }
                }
            }
    }

    /// The heart of it: search a buffer, then try every decoder that applies.
    fn scan_buffer(&mut self, data: &[u8], loc: &Loc, depth: usize, file: Option<&Path>) {
        if self.stop() || data.is_empty() {
            return;
        }
        let cfg = self.w.cfg;
        let can_decode = cfg.decode && depth < cfg.max_depth && (data.len() as u64) <= cfg.max_decode;
        let fmt = if can_decode { formats::detect(data) } else { Format::Other };

        // Containers that decode fully replace the raw pass (no duplicate hits);
        // if they fail to open we fall through and scan the bytes raw.
        let structured = match fmt {
            Format::BPlist => self.scan_bplist(data, loc, depth),
            f if f.is_container() => self.scan_children(f, data, loc, depth),
            _ => false,
        };
        if structured {
            return;
        }

        let live_cells = match (fmt, file) {
            (Format::Sqlite, Some(p)) => self.scan_sqlite(p, loc, depth),
            _ => None,
        };

        let texty = text::is_texty(data);
        let blobs = if can_decode && texty { text::base64_blobs(data, 64, 5000) } else { Vec::new() };

        // ── raw pass ──
        let mut matches = Vec::new();
        let budget = cfg.max_hits_per_file.saturating_sub(self.hits) + 1;
        self.w.m.for_each(data, |r| {
            matches.push(r);
            matches.len() < budget * 4
        });
        let mut covered: Vec<(usize, usize)> = Vec::with_capacity(matches.len());
        let (mut line, mut line_pos) = (1u64, 0usize);
        for r in matches {
            if self.stop() {
                return;
            }
            covered.push((r.start, r.end));
            if r.kind == Kind::Base64 && blobs.iter().any(|(s, e, _)| r.start >= *s && r.end <= *e) {
                continue; // the decoded blob scan below reports it with readable context
            }
            let ctx = render_context(data, r.start, r.end, r.kind, cfg.context);
            let mut detail = kind_detail(data, r).or_else(|| physical(fmt, data, r.start));
            if let Some(cells) = &live_cells {
                if is_live(&ctx, cells) {
                    continue; // already reported from the table row
                }
                detail = Some(format!(
                    "{} · outside live rows (deleted, free space, index or journal)",
                    detail.unwrap_or_default()
                ));
            }
            let ln = if texty {
                line += data[line_pos..r.start].iter().filter(|&&b| b == b'\n').count() as u64;
                line_pos = r.start;
                Some(line)
            } else {
                None
            };
            if !self.emit(loc, r.kind.label().to_string(), r.start, ln, detail, ctx) {
                return;
            }
        }

        if !can_decode {
            return;
        }

        // ── embedded base64 blobs ──
        for (s, _e, dec) in &blobs {
            let label = format!("base64@0x{s:x}");
            let child = loc.child(Seg::Owned(label));
            self.scan_buffer(dec, &child, depth + 1, None);
            if self.stop() {
                return;
            }
        }

        // ── derived text views (markup, quoted-printable, url, escapes, pdf, mime, rtf) ──
        if texty && data.len() <= VIEW_LIMIT {
            covered.sort_unstable();
            let base_len = covered.len();
            for view in text::derived_views(data) {
                let mut found = Vec::new();
                self.w.m.for_each(&view.data, |r| {
                    found.push(r);
                    found.len() < budget * 4
                });
                if found.is_empty() {
                    continue;
                }
                let child = loc.child(Seg::Str(view.name));
                for r in found {
                    let (os, oe) = view.orig_range(r.start, r.end);
                    if overlaps(&covered[..base_len], os, oe) || covered[base_len..].iter().any(|&(a, b)| a < oe && os < b) {
                        continue;
                    }
                    covered.push((os, oe));
                    let ctx = render_context(&view.data, r.start, r.end, r.kind, cfg.context);
                    let detail = kind_detail(&view.data, r);
                    if !self.emit(&child, r.kind.label().to_string(), os, None, detail, ctx) {
                        return;
                    }
                }
            }
        }

        // ── documents & carving ──
        match fmt {
            Format::Pdf => {
                for (off, dec) in archive::pdf_streams(data, cfg.decomp_cap, 20_000) {
                    let child = loc.child(Seg::Owned(format!("pdf-stream@0x{off:x}")));
                    self.scan_buffer(&dec, &child, depth + 1, None);
                    if self.stop() {
                        return;
                    }
                }
            }
            // gzip streams hidden inside binaries (caches, blobs, journals)
            Format::Other | Format::SqliteWal if !texty => {
                for (off, dec) in compress::carve_gzip(data, cfg.decomp_cap, 2_000) {
                    let child = loc.child(Seg::Owned(format!("gzip@0x{off:x}")));
                    self.scan_buffer(&dec, &child, depth + 1, None);
                    if self.stop() {
                        return;
                    }
                }
            }
            _ => {}
        }
    }

    /// Streams, archives and other containers: scan every child they yield.
    fn scan_children(&mut self, fmt: Format, data: &[u8], loc: &Loc, depth: usize) -> bool {
        let cap = self.w.cfg.decomp_cap;
        let entry_method = format!("{} entry name", fmt.label());
        formats::children(fmt, data, cap, &mut |child| {
            if let Some(name) = &child.name {
                self.check_name(name, loc, &entry_method);
            }
            let l = loc.child(Seg::Owned(child.label));
            if fmt.is_archive() {
                self.set_inner(&l);
            }
            self.scan_buffer(&child.data, &l, depth + 1, None);
            !self.stop()
        })
    }

    // ── binary plists ──

    fn scan_bplist(&mut self, data: &[u8], loc: &Loc, depth: usize) -> bool {
        let Ok(v) = plist::Value::from_reader(Cursor::new(data)) else { return false };
        let mut path = Vec::new();
        self.walk_plist(&v, &mut path, loc, depth);
        true
    }

    fn walk_plist(&mut self, v: &plist::Value, path: &mut Vec<String>, loc: &Loc, depth: usize) {
        use plist::Value as V;
        if self.stop() || path.len() > 64 {
            return;
        }
        let here = |path: &Vec<String>| format!("plist:/{}", path.join("/"));
        match v {
            V::Dictionary(d) => {
                for (k, val) in d {
                    if self.w.m.is_match(k.as_bytes()) {
                        let l = here(path);
                        self.check_name(k, &loc.child(Seg::Owned(l)), "plist key");
                    }
                    path.push(k.clone());
                    self.walk_plist(val, path, loc, depth);
                    path.pop();
                }
            }
            V::Array(a) => {
                for (i, val) in a.iter().enumerate() {
                    path.push(format!("[{i}]"));
                    self.walk_plist(val, path, loc, depth);
                    path.pop();
                }
            }
            V::String(s) => {
                let child = loc.child(Seg::Owned(here(path)));
                self.scan_buffer(s.as_bytes(), &child, depth + 1, None);
            }
            V::Data(b) => {
                let child = loc.child(Seg::Owned(here(path)));
                self.scan_buffer(b, &child, depth + 1, None);
            }
            V::Integer(i) => {
                let n = i.as_signed().map(|x| x as f64).or(i.as_unsigned().map(|x| x as f64)).unwrap_or(0.0);
                let key = path.last().map(String::as_str).unwrap_or("");
                self.scan_number(&i.to_string(), n, time::name_hints_time(key), loc, path);
            }
            V::Real(f) => {
                let key = path.last().map(String::as_str).unwrap_or("");
                self.scan_number(&f.to_string(), *f, time::name_hints_time(key), loc, path);
            }
            V::Date(d) => {
                let st: SystemTime = (*d).into();
                let utc: chrono::DateTime<chrono::Utc> = st.into();
                let local: chrono::DateTime<chrono::Local> = st.into();
                let s = format!("{} ⟨{} UTC · {} local⟩", d.to_xml_format(), utc.format("%Y-%m-%d %H:%M:%S"), local.format("%Y-%m-%d %H:%M:%S"));
                let child = loc.child(Seg::Owned(here(path)));
                self.scan_first(s.as_bytes(), &child);
            }
            _ => {}
        }
    }

    fn scan_number(&mut self, text: &str, v: f64, hinted: bool, loc: &Loc, path: &[String]) {
        let s = match time::timestamp_notes(v, hinted) {
            Some(n) => format!("{text} {n}"),
            None => text.to_string(),
        };
        if self.w.m.is_match(s.as_bytes()) {
            let child = loc.child(Seg::Owned(format!("plist:/{}", path.join("/"))));
            self.scan_first(s.as_bytes(), &child);
        }
    }

    // ── sqlite ──

    /// Scan every cell of every table. Returns the (normalized) text of cells
    /// that produced hits, so the raw pass can tell live rows from leftovers.
    fn scan_sqlite(&mut self, path: &Path, loc: &Loc, depth: usize) -> Option<Vec<String>> {
        use rusqlite::{types::ValueRef, Connection, OpenFlags};
        let p = path.to_string_lossy().replace('%', "%25").replace('?', "%3f").replace('#', "%23");
        let uri = format!("file:{p}?mode=ro&immutable=1");
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(&uri, flags).ok()?;
        let tables: Vec<String> = {
            let mut st = conn.prepare("SELECT name FROM sqlite_master WHERE type='table'").ok()?;
            st.query_map([], |r| r.get::<_, String>(0)).ok()?.filter_map(Result::ok).collect()
        };
        let mut live = Vec::new();
        for t in &tables {
            if self.stop() {
                break;
            }
            let tl = format!("sqlite:{t}");
            self.set_inner(&loc.child(Seg::Str(&tl)));
            let qt = t.replace('"', "\"\"");
            let (mut stmt, has_rowid) = match conn.prepare(&format!("SELECT rowid, * FROM \"{qt}\"")) {
                Ok(s) => (s, true),
                Err(_) => match conn.prepare(&format!("SELECT * FROM \"{qt}\"")) {
                    Ok(s) => (s, false),
                    Err(_) => continue,
                },
            };
            let names: Vec<String> = stmt.column_names().map(|s| s.to_string()).collect();
            let first = usize::from(has_rowid);
            let Ok(mut rows) = stmt.query([]) else { continue };
            let mut n = 0u64;
            while let Ok(Some(row)) = rows.next() {
                n += 1;
                if n.is_multiple_of(2048) && self.stop() {
                    break;
                }
                let rowid = if has_rowid { row.get::<_, i64>(0).ok() } else { None };
                for (ci, col) in names.iter().enumerate().skip(first) {
                    let cell = loc.child(Seg::Cell { table: t, rowid, col });
                    let before = self.hits;
                    match row.get_ref(ci) {
                        Ok(ValueRef::Text(b)) => {
                            self.scan_buffer(b, &cell, depth + 1, None);
                            if self.hits > before {
                                live.push(clean(&String::from_utf8_lossy(b)).to_lowercase());
                            }
                        }
                        Ok(ValueRef::Blob(b)) => {
                            self.scan_buffer(b, &cell, depth + 1, None);
                            if self.hits > before {
                                live.push(clean(&String::from_utf8_lossy(b)).to_lowercase());
                            }
                        }
                        Ok(ValueRef::Integer(i)) => {
                            let s = match time::timestamp_notes(i as f64, time::name_hints_time(col)) {
                                Some(note) => format!("{i} {note}"),
                                None => i.to_string(),
                            };
                            if self.w.m.is_match(s.as_bytes()) {
                                self.scan_first(s.as_bytes(), &cell);
                            }
                        }
                        Ok(ValueRef::Real(f)) => {
                            let s = match time::timestamp_notes(f, time::name_hints_time(col)) {
                                Some(note) => format!("{f} {note}"),
                                None => f.to_string(),
                            };
                            if self.w.m.is_match(s.as_bytes()) {
                                self.scan_first(s.as_bytes(), &cell);
                            }
                        }
                        _ => {}
                    }
                    if self.stop() {
                        return Some(live);
                    }
                }
            }
        }
        Some(live)
    }
}

/// Extra explanation for matches whose raw form isn't self-explanatory.
fn kind_detail(data: &[u8], r: crate::matcher::RawMatch) -> Option<String> {
    match r.kind {
        Kind::Base64 => text::base64_run_text(data, r.start, r.end).map(|t| format!("decodes to “{t}”")),
        Kind::Timestamp => {
            let n: f64 = std::str::from_utf8(&data[r.start..r.end]).ok()?.parse().ok()?;
            time::timestamp_notes(n, true)
        }
        _ => None,
    }
}

/// Did this raw hit come from a row we already reported structurally?
fn is_live(ctx: &(String, String, String), cells: &[String]) -> bool {
    if cells.is_empty() {
        return false;
    }
    let tail = |s: &str, n: usize| s.chars().rev().take(n).collect::<Vec<_>>().into_iter().rev().collect::<String>();
    let head = |s: &str, n: usize| s.chars().take(n).collect::<String>();
    let m = ctx.1.to_lowercase();
    let left = format!("{}{}", tail(&ctx.0, 6), m).to_lowercase();
    let right = format!("{}{}", m, head(&ctx.2, 6)).to_lowercase();
    cells.iter().any(|c| c.contains(&left) || c.contains(&right))
}

fn overlaps(sorted: &[(usize, usize)], s: usize, e: usize) -> bool {
    let i = sorted.partition_point(|&(a, _)| a < e);
    sorted[..i].iter().rev().take(64).any(|&(_, b)| b > s)
}

/// Physical position details for database files.
fn physical(fmt: Format, d: &[u8], off: usize) -> Option<String> {
    match fmt {
        Format::Sqlite if d.len() >= 100 => {
            let ps = u16::from_be_bytes([d[16], d[17]]) as usize;
            let ps = if ps == 1 { 65536 } else { ps };
            (ps >= 512).then(|| format!("page {}", off / ps + 1))
        }
        Format::SqliteWal => {
            let ps = u32::from_be_bytes([d[8], d[9], d[10], d[11]]) as usize;
            if !(512..=65536).contains(&ps) || off < 32 {
                return None;
            }
            let frame = 24 + ps;
            let idx = (off - 32) / frame;
            let fs = 32 + idx * frame;
            let pg = d.get(fs..fs + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))?;
            Some(format!("WAL frame {} → page {pg}", idx + 1))
        }
        _ => None,
    }
}

fn load(path: &Path, size: u64) -> std::io::Result<Data> {
    if size <= READ_LIMIT {
        return std::fs::read(path).map(Data::Vec);
    }
    let f = std::fs::File::open(path)?;
    // SAFETY: read-only mapping; we accept the usual risk of a concurrent truncate.
    let m = unsafe { memmap2::Mmap::map(&f)? };
    Ok(Data::Map(m))
}
