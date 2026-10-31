//! Self-contained, offline HTML report: grouped by file, filterable, with
//! method chips and highlighted matches. Data is embedded as JSON and rendered
//! lazily so even hundreds of thousands of hits stay responsive.

use crate::export::{per_file, RunInfo};
use crate::hit::Hit;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn html(info: &RunInfo, hits: &[Hit], total: u64) -> String {
    // JSON inside <script>: neutralise "</script" and friends
    let data = serde_json::to_string(hits).unwrap_or_else(|_| "[]".into()).replace("</", "<\\/").replace("<!--", "<\\!--");
    let files = per_file(hits).len();
    let truncated = if (hits.len() as u64) < total {
        format!("<div class=warn>Showing the first {} of {} hits. All hits are in hits.csv / hits.jsonl.</div>", hits.len(), total)
    } else {
        String::new()
    };
    let interrupted = if info.interrupted { "<span class=pill-warn>interrupted</span>" } else { "" };
    let variants: String = info.variants.iter().map(|v| format!("<span class=var>{}</span>", esc(v))).collect();
    let roots: String = info.roots.iter().map(|r| format!("<code>{}</code>", esc(r))).collect::<Vec<_>>().join(" ");

    TEMPLATE
        .replace("{{NEEDLE}}", &esc(&info.needle))
        .replace("{{ROOTS}}", &roots)
        .replace("{{VARIANTS}}", &variants)
        .replace("{{STARTED}}", &esc(&info.started))
        .replace("{{DURATION}}", &esc(&info.duration))
        .replace("{{FILES}}", &prism::format_number(info.files))
        .replace("{{BYTES}}", &prism::format_bytes(info.bytes))
        .replace("{{HITS}}", &prism::format_number(total))
        .replace("{{HITFILES}}", &prism::format_number(files as u64))
        .replace("{{ERRORS}}", &prism::format_number(info.errors))
        .replace("{{INTERRUPTED}}", interrupted)
        .replace("{{TRUNCATED}}", &truncated)
        .replace("{{COMMAND}}", &esc(&info.command))
        .replace("{{DATA}}", &data)
}

const TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>file-search · {{NEEDLE}}</title>
<style>
:root{
  --bg:#0d1017; --panel:#141923; --panel2:#1a2030; --line:#252c3d; --text:#d7dbe6; --dim:#7d869c;
  --accent:#7aa2f7; --accent2:#bb9af7; --hit-bg:#e0af68; --hit-fg:#1a1b26; --ok:#9ece6a; --warn:#ff9e64;
  --mono: ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Consolas, monospace;
}
@media (prefers-color-scheme: light){
  :root{ --bg:#f6f7fb; --panel:#ffffff; --panel2:#f0f2f8; --line:#e1e4ee; --text:#1f2430; --dim:#6b7285;
         --accent:#3d59d6; --accent2:#7c3aed; --hit-bg:#ffd166; --hit-fg:#1a1b26; }
}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--text);font:14px/1.5 -apple-system,BlinkMacSystemFont,"Segoe UI",Inter,sans-serif}
header{padding:28px 32px 20px;border-bottom:1px solid var(--line);background:
  radial-gradient(1200px 200px at 0% 0%, color-mix(in srgb,var(--accent) 14%,transparent), transparent),
  radial-gradient(900px 200px at 100% 0%, color-mix(in srgb,var(--accent2) 12%,transparent), transparent)}
.brand{font:600 12px var(--mono);letter-spacing:.2em;text-transform:uppercase;color:var(--dim)}
h1{margin:6px 0 10px;font-size:26px;font-weight:650;word-break:break-word}
h1 .q{background:linear-gradient(90deg,var(--accent),var(--accent2));-webkit-background-clip:text;background-clip:text;color:transparent}
.meta{color:var(--dim);font-size:13px}
.meta code{font:12px var(--mono);background:var(--panel2);border:1px solid var(--line);padding:1px 6px;border-radius:5px;color:var(--text)}
.var{display:inline-block;font:11px var(--mono);border:1px solid var(--line);border-radius:999px;padding:1px 8px;margin:2px 4px 2px 0;color:var(--dim)}
.stats{display:grid;grid-template-columns:repeat(auto-fit,minmax(140px,1fr));gap:12px;margin-top:18px}
.stat{background:var(--panel);border:1px solid var(--line);border-radius:12px;padding:12px 14px}
.stat b{display:block;font-size:22px;font-weight:650}
.stat span{color:var(--dim);font-size:12px;text-transform:uppercase;letter-spacing:.06em}
.stat.hl b{color:var(--hit-bg)}
.pill-warn{background:var(--warn);color:#1a1b26;border-radius:999px;padding:1px 9px;font-size:12px;font-weight:600;margin-left:8px}
.warn{margin:14px 0 0;color:var(--warn)}
.toolbar{position:sticky;top:0;z-index:5;display:flex;gap:10px;flex-wrap:wrap;align-items:center;padding:12px 32px;background:color-mix(in srgb,var(--bg) 88%,transparent);backdrop-filter:blur(8px);border-bottom:1px solid var(--line)}
.toolbar input{flex:1;min-width:240px;background:var(--panel);border:1px solid var(--line);color:var(--text);padding:9px 12px;border-radius:10px;font:13px var(--mono);outline:none}
.toolbar input:focus{border-color:var(--accent)}
.chip{cursor:pointer;user-select:none;font:12px var(--mono);border:1px solid var(--line);background:var(--panel);color:var(--dim);border-radius:999px;padding:4px 10px}
.chip.on{color:var(--text);border-color:var(--accent);background:color-mix(in srgb,var(--accent) 16%,var(--panel))}
.btn{cursor:pointer;font:12px var(--mono);border:1px solid var(--line);background:var(--panel);color:var(--text);border-radius:8px;padding:5px 10px}
main{padding:18px 32px 60px}
.count{color:var(--dim);font-size:13px;margin:0 0 12px}
details.file{background:var(--panel);border:1px solid var(--line);border-radius:12px;margin:0 0 10px;overflow:hidden}
details.file>summary{list-style:none;cursor:pointer;display:flex;gap:12px;align-items:center;padding:11px 14px}
details.file>summary::-webkit-details-marker{display:none}
details.file>summary:hover{background:var(--panel2)}
.caret{color:var(--dim);transition:transform .15s;font-size:11px}
details[open] .caret{transform:rotate(90deg)}
.path{font:13px var(--mono);word-break:break-all;flex:1}
.path .dir{color:var(--dim)}
.badge{font:12px var(--mono);background:color-mix(in srgb,var(--hit-bg) 22%,transparent);color:var(--hit-bg);border-radius:999px;padding:1px 9px;white-space:nowrap}
.fmeta{color:var(--dim);font:12px var(--mono);white-space:nowrap}
.hit{border-top:1px solid var(--line);padding:9px 14px 10px 38px}
.hit .top{display:flex;gap:8px;flex-wrap:wrap;align-items:center;font:12px var(--mono);color:var(--dim)}
.m{color:var(--accent2);border:1px solid color-mix(in srgb,var(--accent2) 40%,transparent);border-radius:6px;padding:0 6px}
.loc{color:var(--accent)}
.ctx{margin-top:5px;font:13px/1.55 var(--mono);white-space:pre-wrap;word-break:break-all}
mark{background:var(--hit-bg);color:var(--hit-fg);border-radius:3px;padding:0 2px;font-weight:650}
.more{padding:10px 38px;color:var(--dim);font-size:13px}
.more button{margin-left:8px}
footer{color:var(--dim);font:12px var(--mono);padding:0 32px 40px}
.empty{color:var(--dim);text-align:center;padding:60px 0}
</style>
</head>
<body>
<header>
  <div class="brand">◆ file-search report {{INTERRUPTED}}</div>
  <h1>Results for <span class="q">“{{NEEDLE}}”</span></h1>
  <div class="meta">in {{ROOTS}} · started {{STARTED}} · took {{DURATION}}</div>
  <div class="meta" style="margin-top:6px">matched as {{VARIANTS}}</div>
  <div class="stats">
    <div class="stat hl"><b>{{HITS}}</b><span>hits</span></div>
    <div class="stat"><b>{{HITFILES}}</b><span>files with hits</span></div>
    <div class="stat"><b>{{FILES}}</b><span>files scanned</span></div>
    <div class="stat"><b>{{BYTES}}</b><span>data scanned</span></div>
    <div class="stat"><b>{{ERRORS}}</b><span>unreadable</span></div>
  </div>
  {{TRUNCATED}}
</header>
<div class="toolbar">
  <input id="q" placeholder="Filter by path, location, method or context…" autocomplete="off">
  <span id="chips"></span>
  <button class="btn" id="expand">expand all</button>
  <button class="btn" id="collapse">collapse all</button>
</div>
<main>
  <div class="count" id="count"></div>
  <div id="list"></div>
</main>
<footer>{{COMMAND}}</footer>
<script id="data" type="application/json">{{DATA}}</script>
<script>
const HITS = JSON.parse(document.getElementById('data').textContent);
const $ = s => document.querySelector(s);
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const base = m => m.split(' · ').pop();
const methods = [...new Set(HITS.map(h => base(h.method)))].sort();
const active = new Set(methods);
const chips = $('#chips');
methods.forEach(m => {
  const c = document.createElement('span');
  c.className = 'chip on'; c.textContent = m + ' ' + HITS.filter(h => base(h.method) === m).length;
  c.onclick = () => { active.has(m) ? active.delete(m) : active.add(m); c.classList.toggle('on'); render(); };
  chips.appendChild(c);
});
function fmtBytes(n){ const u=['B','KB','MB','GB','TB']; let i=0; while(n>=1024&&i<u.length-1){n/=1024;i++} return (i?n.toFixed(1):n)+' '+u[i]; }
function pathHtml(p){ const i = p.lastIndexOf('/'); return '<span class="dir">'+esc(p.slice(0,i+1))+'</span>'+esc(p.slice(i+1)); }
function hitHtml(h){
  let meta = '<span class="m">'+esc(h.method)+'</span>';
  if (h.location) meta += '<span class="loc">⟫ '+esc(h.location)+'</span>';
  meta += '<span>@'+h.offset+' (0x'+Number(h.offset).toString(16)+')</span>';
  if (h.line) meta += '<span>line '+h.line+'</span>';
  if (h.detail) meta += '<span>'+esc(h.detail)+'</span>';
  meta += '<span>#'+h.id+'</span>';
  return '<div class="hit"><div class="top">'+meta+'</div><div class="ctx">…'+esc(h.before)+'<mark>'+esc(h.matched)+'</mark>'+esc(h.after)+'…</div></div>';
}
const PAGE = 200;
function render(){
  const q = $('#q').value.trim().toLowerCase();
  const groups = new Map();
  let n = 0;
  for (const h of HITS) {
    if (!active.has(base(h.method))) continue;
    if (q && !(h.path+' '+h.location+' '+h.method+' '+h.before+h.matched+h.after).toLowerCase().includes(q)) continue;
    if (!groups.has(h.path)) groups.set(h.path, []);
    groups.get(h.path).push(h); n++;
  }
  $('#count').textContent = n.toLocaleString()+' hits in '+groups.size.toLocaleString()+' files';
  const list = $('#list'); list.innerHTML = '';
  if (!n) { list.innerHTML = '<div class="empty">No hits match the current filter.</div>'; return; }
  const sorted = [...groups.entries()].sort((a,b) => b[1].length - a[1].length || a[0].localeCompare(b[0]));
  const frag = document.createDocumentFragment();
  sorted.forEach(([path, hs], gi) => {
    const d = document.createElement('details'); d.className = 'file';
    const h0 = hs[0];
    d.innerHTML = '<summary><span class="caret">▶</span><span class="path">'+pathHtml(path)+'</span>'+
      '<span class="fmeta">'+(h0.file_size?fmtBytes(h0.file_size):'')+(h0.modified?' · '+esc(h0.modified):'')+'</span>'+
      '<span class="badge">'+hs.length+'</span></summary>';
    let shown = 0;
    const more = () => {
      const chunk = hs.slice(shown, shown+PAGE).map(hitHtml).join('');
      const old = d.querySelector('.more'); if (old) old.remove();
      d.insertAdjacentHTML('beforeend', chunk); shown += PAGE;
      if (shown < hs.length) {
        const m = document.createElement('div'); m.className='more';
        m.innerHTML = (hs.length-shown)+' more <button class="btn">show more</button>';
        m.querySelector('button').onclick = more; d.appendChild(m);
      }
    };
    d.addEventListener('toggle', () => { if (d.open && !shown) more(); });
    if (gi < 8 || q) { d.open = true; }
    frag.appendChild(d);
  });
  list.appendChild(frag);
}
let t; $('#q').addEventListener('input', () => { clearTimeout(t); t = setTimeout(render, 150); });
$('#expand').onclick = () => document.querySelectorAll('details.file').forEach(d => d.open = true);
$('#collapse').onclick = () => document.querySelectorAll('details.file').forEach(d => d.open = false);
render();
</script>
</body>
</html>
"##;
