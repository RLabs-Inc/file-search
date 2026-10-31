//! Argument parsing. Accepts the natural form
//!   file-search ~/Docs,~/Library -find "some text"
//! and long/short GNU style options alike (-find, --find, -f).

use std::path::PathBuf;

use prism::{s, hex};

use crate::export::Fmt;

pub struct Cli {
    pub roots: Vec<String>,
    pub needle: String,
    pub out: Option<PathBuf>,
    pub no_export: bool,
    pub formats: Vec<Fmt>,
    pub case_sensitive: bool,
    pub accent_sensitive: bool,
    pub regex: bool,
    pub hex: bool,
    pub context: usize,
    pub threads: usize,
    pub decode: bool,
    pub max_depth: usize,
    pub max_decode: u64,
    pub max_hits_per_file: usize,
    pub follow_links: bool,
    pub one_fs: bool,
    pub xattrs: bool,
    pub names: bool,
    pub excludes: Vec<String>,
    pub quiet: bool,
    pub per_file_screen: usize,
}

pub enum Parsed {
    Run(Box<Cli>),
    Help,
    Version,
}

fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim().to_ascii_uppercase();
    let (num, mult) = match t.trim_end_matches('B') {
        x if x.ends_with('K') => (&x[..x.len() - 1], 1u64 << 10),
        x if x.ends_with('M') => (&x[..x.len() - 1], 1 << 20),
        x if x.ends_with('G') => (&x[..x.len() - 1], 1 << 30),
        x if x.ends_with('T') => (&x[..x.len() - 1], 1 << 40),
        x => (x, 1),
    };
    num.trim().parse::<f64>().map(|n| (n * mult as f64) as u64).map_err(|_| format!("invalid size `{s}`"))
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if (p == "~" || p.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(p.trim_start_matches('~').trim_start_matches('/'));
        }
    PathBuf::from(p)
}

pub fn parse(args: Vec<String>) -> Result<Parsed, String> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
    let mut c = Cli {
        roots: Vec::new(),
        needle: String::new(),
        out: None,
        no_export: false,
        formats: Fmt::ALL.to_vec(),
        case_sensitive: false,
        accent_sensitive: false,
        regex: false,
        hex: false,
        context: 40,
        threads,
        decode: true,
        max_depth: 6,
        max_decode: 1 << 30,
        max_hits_per_file: 10_000,
        follow_links: false,
        one_fs: false,
        xattrs: true,
        names: true,
        excludes: Vec::new(),
        quiet: false,
        per_file_screen: 25,
    };
    let mut needle: Option<String> = None;
    let mut it = args.into_iter().peekable();
    let value = |it: &mut std::iter::Peekable<std::vec::IntoIter<String>>, flag: &str| {
        it.next().ok_or_else(|| format!("`{flag}` needs a value"))
    };
    while let Some(a) = it.next() {
        // allow --flag=value
        let (flag, inline) = match a.split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f.to_string(), Some(v.to_string())),
            _ => (a.clone(), None),
        };
        let val = |it: &mut std::iter::Peekable<std::vec::IntoIter<String>>| match &inline {
            Some(v) => Ok(v.clone()),
            None => value(it, &flag),
        };
        match flag.as_str() {
            "-h" | "-help" | "--help" => return Ok(Parsed::Help),
            "-V" | "-version" | "--version" => return Ok(Parsed::Version),
            "-find" | "--find" | "-f" => {
                // everything up to the next option is the needle, so unquoted phrases work too
                let mut parts = vec![val(&mut it)?];
                if inline.is_none() {
                    while let Some(n) = it.peek() {
                        let looks_like_path = n.contains(',') && n.split(',').any(|p| expand_tilde(p.trim()).exists())
                            || expand_tilde(n).exists();
                        if (n.starts_with('-') && n.len() > 1) || looks_like_path {
                            break;
                        }
                        parts.push(it.next().unwrap());
                    }
                }
                needle = Some(parts.join(" "));
            }
            "-o" | "-out" | "--out" | "--output" => c.out = Some(expand_tilde(&val(&mut it)?)),
            "--no-export" | "-no-export" => c.no_export = true,
            "--formats" | "-formats" | "--format" => {
                let v = val(&mut it)?;
                let mut f = Vec::new();
                for p in v.split(',').filter(|x| !x.trim().is_empty()) {
                    let x = Fmt::parse(p).ok_or_else(|| format!("unknown format `{p}` (csv, txt, jsonl, html, md)"))?;
                    if !f.contains(&x) {
                        f.push(x);
                    }
                }
                c.formats = f;
            }
            "-c" | "--case" | "--case-sensitive" | "-case" => c.case_sensitive = true,
            "-a" | "--accents" | "--accent-sensitive" => c.accent_sensitive = true,
            "-r" | "--regex" | "-regex" => c.regex = true,
            "-x" | "--hex" | "-hex" => c.hex = true,
            "-C" | "--context" | "-context" => {
                c.context = val(&mut it)?.parse().map_err(|_| "`--context` takes a number".to_string())?
            }
            "-j" | "--threads" | "-threads" => {
                c.threads = val(&mut it)?.parse().map_err(|_| "`--threads` takes a number".to_string())?;
                c.threads = c.threads.max(1);
            }
            "--raw" | "--no-decode" | "-raw" => c.decode = false,
            "--depth" | "--max-depth" => {
                c.max_depth = val(&mut it)?.parse().map_err(|_| "`--depth` takes a number".to_string())?
            }
            "--max-decode" => c.max_decode = parse_size(&val(&mut it)?)?,
            "--max-hits" | "--max-hits-per-file" => {
                c.max_hits_per_file = val(&mut it)?.parse().map_err(|_| "`--max-hits` takes a number".to_string())?
            }
            "-L" | "--follow" | "--follow-links" => c.follow_links = true,
            "--one-fs" | "--same-fs" => c.one_fs = true,
            "--no-xattr" => c.xattrs = false,
            "--no-names" => c.names = false,
            "-e" | "--exclude" | "-exclude" => {
                c.excludes.extend(val(&mut it)?.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
            }
            "-q" | "--quiet" => c.quiet = true,
            "--screen-per-file" => {
                c.per_file_screen = val(&mut it)?.parse().map_err(|_| "`--screen-per-file` takes a number".to_string())?
            }
            f if f.starts_with('-') && f.len() > 1 && !std::path::Path::new(f).exists() => {
                return Err(format!("unknown option `{f}` — see `file-search --help`"));
            }
            _ => {
                for p in a.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    c.roots.push(p.to_string());
                }
            }
        }
    }
    c.needle = needle.ok_or("missing `-find <text>`")?;
    if c.needle.is_empty() {
        return Err("the search text is empty".into());
    }
    if c.roots.is_empty() {
        c.roots.push(".".into());
    }
    Ok(Parsed::Run(Box::new(c)))
}

pub fn help() -> String {
    let a = |t: &str| s().fg(hex(0x7AA2F7)).bold().paint(t);
    let m = |t: &str| s().fg(hex(0xBB9AF7)).paint(t);
    let d = |t: &str| s().dim().paint(t);
    let h = |t: &str| s().bold().paint(t);
    let mut o = String::new();
    o.push_str(&format!(
        "\n  {}  {}\n\n",
        a("◆ file-search"),
        d("find anything, anywhere — inside every file, decoded")
    ));
    o.push_str(&format!("  {}\n    file-search {} {} {} {}\n\n", h("USAGE"), m("<folders,comma,separated>"), a("-find"), m("<anything>"), d("[options]")));
    o.push_str(&format!("  {}\n", h("EXAMPLES")));
    for (cmd, note) in [
        ("file-search ~/Documents,~/Desktop -find \"invoice 2024\"", "words, phrases, whole paragraphs"),
        ("file-search ~/Library -find \"+55 (11) 98765-4321\"", "phone numbers in any formatting"),
        ("file-search ~ -find 2024-03-15", "dates in 20+ notations & db timestamps"),
        ("file-search /Volumes/img -find 0xDEADBEEF", "raw bytes"),
        ("file-search . -find 'user_\\d{4}' --regex", "regular expressions"),
    ] {
        o.push_str(&format!("    {}\n      {}\n", cmd, d(note)));
    }
    o.push_str(&format!("\n  {}\n", h("WHAT IS SEARCHED")));
    for line in [
        "every file & subfolder, hidden ones included · file, folder and archive entry names · extended attributes",
        "text as UTF-8, UTF-16LE/BE, hex text, base64 · digits with any separators · dates in many formats",
        "decoded: gzip bzip2 xz lzma zstd zlib · zip (docx xlsx pptx jar epub…) · tar · binary plists",
        "SQLite (every table/row/cell + leftovers in free pages) · WAL journals (frame/page) · PDF streams",
        "derived text: markup-stripped XML/HTML, quoted-printable mail, URL-encoding, \\u escapes, embedded base64",
    ] {
        o.push_str(&format!("    {} {}\n", m("•"), line));
    }
    o.push_str(&format!("\n  {}\n", h("OPTIONS")));
    for (f, desc) in [
        ("-find, -f <text>", "what to look for (required)"),
        ("-o, --out <dir>", "export folder (default ./file-search-results/<timestamp>)"),
        ("--formats <list>", "csv,txt,jsonl,html,md (default: all)"),
        ("--no-export", "only show results on screen"),
        ("-c, --case", "case-sensitive (default: insensitive)"),
        ("-a, --accents", "accent-sensitive (default: São = Sao = SÃO)"),
        ("-r, --regex", "treat the text as a regular expression"),
        ("-x, --hex", "treat the text as hex bytes (auto for 0x… / hex:…)"),
        ("-C, --context <n>", "characters of context on each side (default 40)"),
        ("-j, --threads <n>", "parallel workers (default: all cores)"),
        ("-e, --exclude <list>", "skip names (node_modules,.git) or path parts (/Caches/)"),
        ("--raw", "no decoding: raw byte search only (fastest)"),
        ("--depth <n>", "max nesting for decoders (default 6)"),
        ("--max-decode <size>", "don't decode containers bigger than this (default 1G)"),
        ("--max-hits <n>", "max hits recorded per file (default 10000)"),
        ("--screen-per-file <n>", "hits printed on screen per file (default 25, all are exported)"),
        ("-L, --follow", "follow symbolic links"),
        ("--one-fs", "stay on the same filesystem"),
        ("--no-xattr / --no-names", "skip extended attributes / name matching"),
        ("-q, --quiet", "no per-hit output, just the live status and summary"),
    ] {
        o.push_str(&format!("    {}{}{}\n", a(f), " ".repeat(26usize.saturating_sub(f.chars().count())), desc));
    }
    o.push_str(&format!("\n  {}\n", d("Ctrl-C stops gracefully and still writes every export. Press twice to abort.")));
    o
}
