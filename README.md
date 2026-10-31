# ◆ file-search

**Find anything, anywhere — inside every file, decoded.**

```
file-search ~/Documents,~/Library -find "anything at all"
```

Give it folders and a string (a word, a whole paragraph, a phone number, a
date, raw bytes) and it digs through every file and subfolder, hidden ones
included. It opens archives, databases, plists, compressed streams and
encoded text, so it finds matches that `grep` can't see. Hits stream into your
scrollback as they're found while a pinned live panel shows what's being
searched right now. Every hit is exported to CSV, TXT, JSONL, HTML and Markdown.

Built in Rust on [rlabs-prism](https://crates.io/crates/rlabs-prism). MIT licensed.

## Install

```sh
cargo install --path .
```

## Usage

```sh
file-search <folders,comma,separated> -find <anything> [options]
```

| Example | Finds |
|---|---|
| `file-search ~/Docs -find "invoice 2024"` | words & paragraphs (case-insensitive and whitespace-flexible, so text that wraps across lines still matches) |
| `file-search ~/Library -find "11 98765-4321"` | phone numbers / IDs in **any** formatting: `(11) 98765-4321`, `11.98765.4321`, `11987654321` |
| `file-search ~ -find 2024-03-15` | dates in 20+ notations (`15/03/2024`, `March 15, 2024`, `15 de março de 2024`…) **and** Unix / Mac / WebKit timestamps in databases & plists |
| `file-search /Volumes/img -find 0xDEADBEEF` | raw bytes (also `hex:de ad be ef` or `--hex`) |
| `file-search . -find 'user_\d{4}' --regex` | regular expressions |

The phrase after `-find` doesn't need quotes. Everything up to the next option is the search text.

## What gets searched

**Every needle is matched as:** UTF-8 text · UTF-16LE · UTF-16BE · hex text
(`50 68 6f…`, `50:68:6f`, `50686f`) · base64 at all 3 byte alignments ·
digits with any separators · date notations.

**Containers & formats are decoded recursively, detected by content (not extension):**

| Kind | What happens |
|---|---|
| gzip, bzip2, xz, lzma, zstd, zlib | decompressed and searched |
| zip (docx, xlsx, pptx, odt, epub, jar, ipa…), tar | every entry searched, plus entry names |
| binary plist | every key / string / number / date walked; `Data` blobs decoded recursively (nested plists, NSKeyedArchiver) |
| SQLite | every table, row and cell (`sqlite:message[rowid 42].text`); blobs decoded; timestamp columns rendered as dates; **raw hits outside live rows are flagged as deleted / free-space leftovers** |
| SQLite WAL | hits mapped to WAL frame and page number |
| PDF | FlateDecode streams inflated; text extracted from `Tj`/`TJ` operators |
| XML / HTML / Office XML | markup stripped and entities decoded, so `<w:t>Phoe</w:t><w:t>nix</w:t>` matches "Phoenix" |
| email | quoted-printable decoded; MIME base64 bodies decoded |
| JSON / code | `\uXXXX`, `\xXX` escapes decoded |
| logs / URLs | percent-encoding decoded |
| anything text | embedded base64 blobs (≥ 64 chars, line breaks allowed) decoded and searched recursively |
| binaries, images, unknown | raw byte scan in every encoding above |

Also searched: **file names, folder names, archive entry names, extended
attributes** (e.g. macOS `com.apple.metadata:kMDItemWhereFroms`).

Hits found by a decoded view are mapped back to the original byte offset and
de-duplicated against the raw scan, so you don't get the same match twice.

## Output

The live panel shows progress, throughput, counters and what every worker is
reading at that moment (including the inner path, e.g. `report.docx ⟫ zip:word/document.xml`).

Each hit in the scrollback shows how it matched, where it sits inside the file,
line or offset, and a small context with the match highlighted.

Exports go to `./file-search-results/<timestamp>_<query>/` (or `-o <dir>`):

| File | |
|---|---|
| `hits.csv` | one row per hit: file, location, method, offset, line, detail, before / match / after, size, mtime |
| `hits.txt` | human-readable listing |
| `hits.jsonl` | one JSON object per hit |
| `report.html` | self-contained report: grouped by file, live filter, method toggles, highlighted context |
| `report.md` | Markdown tables |
| `summary.json` | run metadata and per-file counts |
| `errors.log` | everything that couldn't be read |

CSV/TXT/JSONL are written **while** searching. Ctrl-C stops gracefully and
still writes every report (exit code 130). Press it twice to abort immediately.

## Options

```
-find, -f <text>        what to look for (required)
-o, --out <dir>         export folder
--formats <list>        csv,txt,jsonl,html,md (default: all)
--no-export             screen only
-c, --case              case-sensitive
-r, --regex             regular expression
-x, --hex               hex bytes
-C, --context <n>       context characters per side (default 40)
-j, --threads <n>       workers (default: all cores)
-e, --exclude <list>    skip names (node_modules,.git) or path fragments (/Caches/)
--raw                   no decoding, raw search only
--depth <n>             max decoder nesting (default 6)
--max-decode <size>     largest container to decode (default 1G)
--max-hits <n>          max hits recorded per file (default 10000)
--screen-per-file <n>   hits printed per file (default 25; all are exported)
-L, --follow            follow symlinks
--one-fs                stay on one filesystem
--no-xattr, --no-names  skip xattrs / name matching
-q, --quiet             live panel + summary only
```

## Notes

- Files are only ever **read**. SQLite databases are opened `mode=ro&immutable=1`,
  so WAL/SHM files are never touched or checkpointed.
- Sockets, FIFOs and devices are never opened. Symlinks aren't followed unless you pass `-L`.
- The export folder is automatically excluded from the search.
- On macOS, protected folders (Mail, Messages, Safari…) need *Full Disk Access*
  for your terminal, or `sudo`.
- Not decoded (yet): encrypted archives, Snappy (Chrome LevelDB), PDF text in
  fonts without a Unicode map, OCR of images.

## Development

```sh
cargo test                 # unit tests (matcher shapes, decoders, timestamps)
./scripts/selftest.sh      # end-to-end: hides the needle in 27 formats and checks each is found
```
