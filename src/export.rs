//! Result exporters. CSV / TXT / JSONL are written as hits arrive (so a crash
//! or Ctrl-C never loses results); HTML + summary are rendered at the end.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::hit::Hit;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Fmt {
    Csv,
    Txt,
    Jsonl,
    Html,
    Md,
}

impl Fmt {
    pub fn parse(s: &str) -> Option<Fmt> {
        match s.trim().to_ascii_lowercase().as_str() {
            "csv" => Some(Fmt::Csv),
            "txt" | "text" => Some(Fmt::Txt),
            "json" | "jsonl" | "ndjson" => Some(Fmt::Jsonl),
            "html" | "htm" => Some(Fmt::Html),
            "md" | "markdown" => Some(Fmt::Md),
            _ => None,
        }
    }
    pub const ALL: [Fmt; 5] = [Fmt::Csv, Fmt::Txt, Fmt::Jsonl, Fmt::Html, Fmt::Md];
}

pub struct RunInfo {
    pub needle: String,
    pub roots: Vec<String>,
    pub variants: Vec<String>,
    pub started: String,
    pub duration: String,
    pub files: u64,
    pub bytes: u64,
    pub dirs: u64,
    pub errors: u64,
    pub interrupted: bool,
    pub command: String,
}

pub struct Exporter {
    pub dir: PathBuf,
    formats: Vec<Fmt>,
    csv: Option<csv::Writer<File>>,
    txt: Option<BufWriter<File>>,
    jsonl: Option<BufWriter<File>>,
    errors: BufWriter<File>,
    /// kept for the end-of-run reports
    pub hits: Vec<Hit>,
    pub written: Vec<PathBuf>,
}

const KEEP_FOR_REPORT: usize = 250_000;

impl Exporter {
    pub fn new(dir: &Path, formats: &[Fmt], info_needle: &str) -> std::io::Result<Exporter> {
        std::fs::create_dir_all(dir)?;
        let mut written = Vec::new();
        let mut open = |name: &str| -> std::io::Result<File> {
            let p = dir.join(name);
            written.push(p.clone());
            File::create(p)
        };
        let csv = if formats.contains(&Fmt::Csv) {
            let mut w = csv::Writer::from_writer(open("hits.csv")?);
            w.write_record([
                "id", "file", "location", "method", "offset", "line", "detail", "before", "match", "after",
                "context", "file_size", "modified",
            ])?;
            Some(w)
        } else {
            None
        };
        let txt = if formats.contains(&Fmt::Txt) {
            let mut w = BufWriter::new(open("hits.txt")?);
            writeln!(w, "file-search results for: {info_needle}")?;
            writeln!(w, "{}\n", "=".repeat(78))?;
            Some(w)
        } else {
            None
        };
        let jsonl = if formats.contains(&Fmt::Jsonl) { Some(BufWriter::new(open("hits.jsonl")?)) } else { None };
        let errors = BufWriter::new(open("errors.log")?);
        Ok(Exporter { dir: dir.to_path_buf(), formats: formats.to_vec(), csv, txt, jsonl, errors, hits: Vec::new(), written })
    }

    pub fn hit(&mut self, h: &Hit) {
        if let Some(w) = &mut self.csv {
            let _ = w.write_record([
                h.id.to_string(),
                h.path.clone(),
                h.location.clone(),
                h.method.clone(),
                h.offset.to_string(),
                h.line.map(|l| l.to_string()).unwrap_or_default(),
                h.detail.clone().unwrap_or_default(),
                h.before.clone(),
                h.matched.clone(),
                h.after.clone(),
                h.context(),
                h.file_size.to_string(),
                h.modified.clone().unwrap_or_default(),
            ]);
        }
        if let Some(w) = &mut self.txt {
            let _ = writeln!(w, "[#{}] {}", h.id, h.path);
            if !h.location.is_empty() {
                let _ = writeln!(w, "      in: {}", h.location);
            }
            let mut meta = format!("      via {} @ offset {} (0x{:x})", h.method, h.offset, h.offset);
            if let Some(l) = h.line {
                meta.push_str(&format!(", line {l}"));
            }
            if let Some(d) = &h.detail {
                meta.push_str(&format!(", {d}"));
            }
            let _ = writeln!(w, "{meta}");
            let _ = writeln!(w, "      …{}[[{}]]{}…\n", h.before, h.matched, h.after);
        }
        if let Some(w) = &mut self.jsonl
            && let Ok(s) = serde_json::to_string(h) {
                let _ = writeln!(w, "{s}");
            }
        if self.hits.len() < KEEP_FOR_REPORT {
            self.hits.push(h.clone());
        }
    }

    pub fn error(&mut self, path: &str, msg: &str) {
        let _ = writeln!(self.errors, "{path}\t{msg}");
    }

    pub fn flush(&mut self) {
        if let Some(w) = &mut self.csv {
            let _ = w.flush();
        }
        if let Some(w) = &mut self.txt {
            let _ = w.flush();
        }
        if let Some(w) = &mut self.jsonl {
            let _ = w.flush();
        }
        let _ = self.errors.flush();
    }

    pub fn finish(&mut self, info: &RunInfo, total_hits: u64) -> std::io::Result<()> {
        self.flush();
        // workers interleave files; reports read better grouped
        self.hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.id.cmp(&b.id)));
        let per_file = per_file(&self.hits);
        let summary = serde_json::json!({
            "query": info.needle,
            "folders": info.roots,
            "variants": info.variants,
            "command": info.command,
            "started": info.started,
            "duration": info.duration,
            "interrupted": info.interrupted,
            "files_scanned": info.files,
            "folders_scanned": info.dirs,
            "bytes_scanned": info.bytes,
            "errors": info.errors,
            "hits": total_hits,
            "files_with_hits": per_file.len(),
            "per_file": per_file.iter().map(|(p, n)| serde_json::json!({"file": p, "hits": n})).collect::<Vec<_>>(),
        });
        let p = self.dir.join("summary.json");
        std::fs::write(&p, serde_json::to_string_pretty(&summary)?)?;
        self.written.push(p);
        if self.formats.contains(&Fmt::Html) {
            let p = self.dir.join("report.html");
            std::fs::write(&p, crate::report::html(info, &self.hits, total_hits))?;
            self.written.push(p);
        }
        if self.formats.contains(&Fmt::Md) {
            let p = self.dir.join("report.md");
            std::fs::write(&p, markdown(info, &self.hits, total_hits))?;
            self.written.push(p);
        }
        Ok(())
    }
}

pub fn per_file(hits: &[Hit]) -> Vec<(String, usize)> {
    let mut m: BTreeMap<&str, usize> = BTreeMap::new();
    for h in hits {
        *m.entry(&h.path).or_default() += 1;
    }
    let mut v: Vec<(String, usize)> = m.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    v
}

fn md_escape(s: &str) -> String {
    s.replace('|', "\\|").replace('`', "'")
}

fn markdown(info: &RunInfo, hits: &[Hit], total: u64) -> String {
    let mut s = String::new();
    s.push_str(&format!("# file-search report\n\n**Query:** `{}`  \n", md_escape(&info.needle)));
    s.push_str(&format!("**Folders:** {}  \n", info.roots.join(", ")));
    s.push_str(&format!("**Started:** {} · **Duration:** {}{}  \n", info.started, info.duration, if info.interrupted { " · ⚠ interrupted" } else { "" }));
    s.push_str(&format!(
        "**Scanned:** {} files · {} · **Hits:** {} in {} files · **Errors:** {}\n\n",
        info.files,
        prism::format_bytes(info.bytes),
        total,
        per_file(hits).len(),
        info.errors
    ));
    let mut current = "";
    for h in hits {
        if h.path != current {
            current = &h.path;
            s.push_str(&format!("\n## `{}`\n\n| # | where | via | offset | context |\n|---|---|---|---|---|\n", md_escape(&h.path)));
        }
        s.push_str(&format!(
            "| {} | {} | {} | {} | …{}**{}**{}… |\n",
            h.id,
            md_escape(&h.location),
            md_escape(&h.method),
            h.offset,
            md_escape(&h.before),
            md_escape(&h.matched),
            md_escape(&h.after)
        ));
    }
    s
}
