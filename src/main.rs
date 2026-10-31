mod cli;
mod decode;
mod export;
mod hit;
mod matcher;
mod report;
mod scan;
mod ui;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, unbounded, RecvTimeoutError};
use prism::{format_bytes, format_number, frame, link, BorderStyle, FrameOptions, LayoutOptions};

use cli::Parsed;
use export::{Exporter, RunInfo};
use matcher::{Matcher, MatcherOptions};
use scan::{Config, Event, Slot, Stats, Worker};
use ui::*;

fn fail(msg: &str) -> ! {
    eprintln!("\n  {} {}\n", cb(RED, "✗"), msg);
    std::process::exit(2);
}

fn slug(s: &str) -> String {
    let mut out: String = s
        .chars()
        .map(|c| if c.is_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    out = out.chars().take(32).collect();
    if out.is_empty() { "query".into() } else { out }
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let command = format!("file-search {}", argv.iter().map(|a| if a.contains(' ') { format!("{a:?}") } else { a.clone() }).collect::<Vec<_>>().join(" "));
    let cli = match cli::parse(argv) {
        Ok(Parsed::Run(c)) => c,
        Ok(Parsed::Help) => {
            print!("{}", cli::help());
            return;
        }
        Ok(Parsed::Version) => {
            println!("file-search {}", env!("CARGO_PKG_VERSION"));
            return;
        }
        Err(e) => {
            print!("{}", cli::help());
            fail(&e);
        }
    };

    // ── matcher ──
    let m = Matcher::new(&cli.needle, &MatcherOptions { case_sensitive: cli.case_sensitive, regex: cli.regex, hex: cli.hex })
        .unwrap_or_else(|e| fail(&e));
    let variants: Vec<String> = m.variants.iter().map(|(_, d)| d.clone()).collect();

    // ── folders ──
    let mut roots = Vec::new();
    let mut missing = Vec::new();
    for r in &cli.roots {
        match cli::expand_tilde(r).canonicalize() {
            Ok(p) => {
                if !roots.contains(&p) {
                    roots.push(p)
                }
            }
            Err(e) => missing.push(format!("{r}: {e}")),
        }
    }
    if !missing.is_empty() {
        fail(&format!("can't open: {}", missing.join(", ")));
    }

    // ── export folder ──
    let started_at = chrono::Local::now();
    let out_dir: Option<PathBuf> = if cli.no_export {
        None
    } else {
        let d = cli.out.clone().unwrap_or_else(|| {
            PathBuf::from("file-search-results").join(format!("{}_{}", started_at.format("%Y-%m-%d_%H-%M-%S"), slug(&cli.needle)))
        });
        if let Err(e) = std::fs::create_dir_all(&d) {
            fail(&format!("can't create export folder {}: {e}", d.display()));
        }
        Some(d.canonicalize().unwrap_or(d))
    };
    let mut exporter = out_dir.as_ref().map(|d| {
        Exporter::new(d, &cli.formats, &cli.needle).unwrap_or_else(|e| fail(&format!("can't write exports: {e}")))
    });

    let cfg = Config {
        roots: roots.clone(),
        context: cli.context,
        decode: cli.decode,
        max_depth: cli.max_depth,
        max_decode: cli.max_decode,
        decomp_cap: 512 << 20,
        max_hits_per_file: cli.max_hits_per_file,
        follow_links: cli.follow_links,
        one_fs: cli.one_fs,
        xattrs: cli.xattrs,
        names: cli.names,
        excludes: cli.excludes.clone(),
        out_dir: out_dir.clone(),
        threads: cli.threads,
    };

    let root_strs: Vec<String> = roots.iter().map(|r| r.display().to_string()).collect();
    print!("{}", header(&cli.needle, &root_strs, &variants, out_dir.as_ref().map(|d| d.to_str().unwrap_or("")), cfg.threads));

    // ── shared state ──
    let stats = Arc::new(Stats::default());
    let slots: Arc<Vec<Mutex<Slot>>> = Arc::new((0..cfg.threads).map(|_| Mutex::new(Slot::default())).collect());
    let started = Instant::now();

    {
        let st = Arc::clone(&stats);
        let _ = ctrlc::set_handler(move || {
            if st.cancel.swap(true, Relaxed) {
                prism::show_cursor();
                eprintln!("\n  aborted.");
                std::process::exit(130);
            }
        });
    }
    scan::install_panic_hook();

    let ly = prism::layout(Some(LayoutOptions { tty: Some(live_tty()), ..Default::default() }));
    let tty = live_tty();
    if tty {
        prism::hide_cursor();
    }
    {
        let (st, sl) = (Arc::clone(&stats), Arc::clone(&slots));
        let tick0 = Instant::now();
        ly.set_active(Box::new(move || {
            let tick = (tick0.elapsed().as_millis() / 80) as usize;
            live_frame(&st, &sl, started, tick)
        }));
    }

    let (job_tx, job_rx) = bounded::<(PathBuf, u64)>(100_000);
    let (ev_tx, ev_rx) = unbounded::<Event>();

    let mut next_id = 0u64;
    let mut per_file_shown: HashMap<String, usize> = HashMap::new();
    let mut top: HashMap<String, u64> = HashMap::new();
    let mut last_path = String::new();
    let mut error_samples: Vec<(String, String)> = Vec::new();
    let mut permission_errors = 0u64;

    std::thread::scope(|sc| {
        {
            let (cfg, m, stats, ev) = (&cfg, &m, &*stats, ev_tx.clone());
            sc.spawn(move || scan::walk(cfg, m, stats, job_tx, ev));
        }
        for i in 0..cfg.threads {
            let rx = job_rx.clone();
            let w = Worker { m: &m, cfg: &cfg, stats: &stats, slot: &slots[i], ev: ev_tx.clone() };
            sc.spawn(move || {
                while let Ok((path, size)) = rx.recv() {
                    if w.stats.cancel.load(Relaxed) {
                        continue; // drain quickly
                    }
                    w.run(path, size);
                }
            });
        }
        drop(ev_tx);
        drop(job_rx);

        // ── UI / collector loop (main thread) ──
        let mut pending: Vec<String> = Vec::new();
        let mut last_draw = Instant::now();
        let mut last_flush = Instant::now();
        loop {
            let first = ev_rx.recv_timeout(Duration::from_millis(40));
            let mut batch = Vec::new();
            match first {
                Ok(e) => batch.push(e),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            while batch.len() < 5000 {
                match ev_rx.try_recv() {
                    Ok(e) => batch.push(e),
                    Err(_) => break,
                }
            }
            for e in batch {
                match e {
                    Event::Hit(mut h) => {
                        next_id += 1;
                        h.id = next_id;
                        *top.entry(h.path.clone()).or_default() += 1;
                        if let Some(x) = exporter.as_mut() {
                            x.hit(&h);
                        }
                        if cli.quiet {
                            continue;
                        }
                        let shown = per_file_shown.entry(h.path.clone()).or_default();
                        *shown += 1;
                        if *shown > cli.per_file_screen {
                            if *shown == cli.per_file_screen + 1 {
                                pending.push(more_line(&h.path, cli.per_file_screen));
                            }
                            continue;
                        }
                        if h.path != last_path {
                            pending.push(file_header(&h));
                            last_path = h.path.clone();
                        }
                        pending.push(hit_lines(&h));
                    }
                    Event::Error { path, msg } => {
                        if msg.contains("ermission denied") || msg.contains("not permitted") {
                            permission_errors += 1;
                        }
                        if error_samples.len() < 5 {
                            error_samples.push((path.clone(), msg.clone()));
                        }
                        if let Some(x) = exporter.as_mut() {
                            x.error(&path, &msg);
                        }
                    }
                    Event::Capped { path, limit } => pending.push(capped_line(&path, limit)),
                }
            }
            if last_draw.elapsed() >= Duration::from_millis(80) {
                if !pending.is_empty() {
                    ly.print(&pending.join("\n"));
                    pending.clear();
                }
                ly.refresh();
                last_draw = Instant::now();
            }
            if last_flush.elapsed() >= Duration::from_secs(1) {
                if let Some(x) = exporter.as_mut() {
                    x.flush();
                }
                last_flush = Instant::now();
            }
        }
        if !pending.is_empty() {
            ly.print(&pending.join("\n"));
        }
    });

    ly.close(None);
    if tty {
        prism::show_cursor();
    }

    // ── summary ──
    let el = started.elapsed().as_secs_f64();
    let interrupted = stats.cancel.load(Relaxed);
    let files = stats.files_done.load(Relaxed);
    let bytes = stats.bytes_done.load(Relaxed);
    let hits = stats.hits.load(Relaxed);
    let fh = stats.files_hit.load(Relaxed);
    let errs = stats.errors.load(Relaxed);
    let dirs = stats.dirs.load(Relaxed);

    let info = RunInfo {
        needle: cli.needle.clone(),
        roots: root_strs.clone(),
        variants: variants.clone(),
        started: started_at.format("%Y-%m-%d %H:%M:%S").to_string(),
        duration: duration(el),
        files,
        bytes,
        dirs,
        errors: errs,
        interrupted,
        command,
    };
    let mut export_err = None;
    if let Some(x) = exporter.as_mut()
        && let Err(e) = x.finish(&info, hits) {
            export_err = Some(e.to_string());
        }

    let w = (term_width() as usize).clamp(60, 110);
    let mut body = String::new();
    let headline = if interrupted {
        format!("{} stopped after {}", cb(ORANGE, "■"), duration(el))
    } else {
        format!("{} done in {}", cb(GREEN, "✓"), duration(el))
    };
    let hits_s = if hits > 0 {
        format!("{} {} in {} {}", cb(GOLD, &format_number(hits)), if hits == 1 { "hit" } else { "hits" }, bold(&format_number(fh)), if fh == 1 { "file" } else { "files" })
    } else {
        dim("no hits")
    };
    body.push_str(&format!("{headline}  ·  {hits_s}\n\n"));
    let rate = if el > 0.0 { format!("{}/s", format_bytes((bytes as f64 / el) as u64)) } else { String::new() };
    body.push_str(&format!(
        "{}  {} files · {} folders · {} {}\n",
        dim("scanned "),
        bold(&format_number(files)),
        format_number(dirs),
        bold(&format_bytes(bytes)),
        dim(&format!("({rate})"))
    ));
    if errs > 0 {
        body.push_str(&format!("{}  {} {}\n", dim("unread  "), c(ORANGE, &format_number(errs)), dim(if exporter.is_some() { "items couldn't be read (see errors.log)" } else { "items couldn't be read" })));
        for (p, msg) in error_samples.iter().take(3) {
            body.push_str(&format!("{}  {} {}\n", dim("        "), dim(&mid_trunc(&home_short(p), w.saturating_sub(50))), dim(&format!("— {msg}"))));
        }
        if permission_errors > 0 && cfg!(target_os = "macos") {
            body.push_str(&format!("{}  {}\n", dim("        "), c(ORANGE, "tip: grant your terminal Full Disk Access, or use sudo")));
        }
    }
    if !top.is_empty() {
        let mut tv: Vec<(&String, &u64)> = top.iter().collect();
        tv.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        body.push_str(&format!("\n{}\n", dim("top files")));
        for (p, n) in tv.iter().take(8) {
            body.push_str(&format!("  {:>7}  {}\n", cb(GOLD, &format_number(**n)), mid_trunc(&home_short(p), w.saturating_sub(18))));
        }
        if tv.len() > 8 {
            body.push_str(&format!("  {}\n", dim(&format!("… and {} more files", tv.len() - 8))));
        }
    }
    if let Some(x) = &exporter {
        let dir = x.dir.display().to_string();
        body.push_str(&format!("\n{}  {}\n", dim("exports"), c(CYAN, &mid_trunc(&home_short(&dir), w.saturating_sub(16)))));
        let names: Vec<String> = x
            .written
            .iter()
            .map(|p| {
                let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                if tty { c(CYAN, &link(&name, &format!("file://{}", p.display()), true)) } else { name }
            })
            .collect();
        body.push_str(&format!("  {}\n", names.join(&dim("  ·  "))));
    }
    if let Some(e) = export_err {
        body.push_str(&format!("\n{} {}\n", cb(RED, "✗ export failed:"), e));
    }
    let boxed = frame(body.trim_end(), &FrameOptions { border: BorderStyle::Rounded, width: Some(w), padding: 1, title: Some(" file-search ".into()) });
    println!("\n{boxed}\n");
    if interrupted {
        std::process::exit(130);
    }
}
