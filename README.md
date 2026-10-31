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

### How the text is matched

| | |
|---|---|
| **Case & accents** | case- and accent-insensitive by default: `São Paulo` = `Sao Paulo` = `SÃO PAULO` (`-c` / `--accents` to be strict) |
| **Unicode forms** | precomposed (NFC) and decomposed (NFD, e.g. macOS file names) both match |
| **Encodings** | UTF-8 · UTF-16LE · UTF-16BE · and legacy code pages chosen from the needle's script: cp1252 & Mac Roman (Western), cp1250/ISO-8859-2 (Central European), cp1254 (Turkish), cp1251/KOI8-R/KOI8-U/IBM866 (Cyrillic), cp1253 (Greek), cp1255 (Hebrew), cp1256 (Arabic), cp874 (Thai), cp1258 (Vietnamese), Shift_JIS/EUC-JP (Japanese), GB18030/Big5 (Chinese), EUC-KR (Korean) |
| **Encoded forms** | hex text (`50 68 6f…`, `50:68:6f`, `50686f`) · base64 at all 3 byte alignments (shown decoded) |
| **Numbers** | digits with any separators: `11987654321` = `(11) 98765-4321` = `11.98765.4321` |
| **Dates** | 20+ notations (`15/03/2024`, `March 15, 2024`, `15 de março de 2024`, EXIF `2024:03:15`…) **and numeric timestamps of that day anywhere**: Unix s/ms/µs, Apple/Cocoa s & ns (2001 epoch), Chrome/WebKit µs, Windows FILETIME, HFS+ (1904 epoch) |

### Formats decoded (recursively, detected by content, never by extension)

| Kind | What happens |
|---|---|
| gzip, bzip2, xz, lzma, zstd, zlib, LZ4, **Apple LZFSE / LZ4** (`compression_tool`), Snappy | decompressed and searched |
| gzip streams **inside** binaries | carved out of caches, blobs and journals and inflated |
| zip (docx, xlsx, pptx, odt, epub, jar, ipa…), tar, **7z**, **cab**, **cpio** | every entry searched, plus entry names |
| **OLE / Compound File** (legacy .doc .xls .ppt, Outlook .msg, Thumbs.db) | every stream reassembled from its sectors (lenient reader: works on files strict parsers reject) |
| binary plist | every key / string / number / date walked; `Data` blobs decoded recursively (nested plists, NSKeyedArchiver) |
| **typedstream** (iMessage `attributedBody`) | message strings extracted |
| SQLite | every table, row and cell (`sqlite:message[rowid 42].text`); blobs decoded; numbers that are timestamps rendered as dates; **raw hits outside live rows are flagged as deleted / free-space leftovers** |
| SQLite WAL | hits mapped to WAL frame and page number |
| **LevelDB / RocksDB** (.ldb/.sst: Chrome, Edge, Electron apps such as Slack, Discord, VS Code, WhatsApp, Teams) | blocks decompressed (Snappy, LZ4, zstd), keys and values searched |
| PDF | FlateDecode streams inflated; text extracted from `Tj`/`TJ` operators |
| **RTF** | control words dropped, `\'hh` (any code page) and `\uN` decoded |
| XML / HTML / Office XML | markup stripped and entities decoded, so `<w:t>Phoe</w:t><w:t>nix</w:t>` matches "Phoenix" |
| email | quoted-printable, MIME base64 bodies, **RFC 2047 headers** (`=?UTF-8?B?…?=`) |
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
-a, --accents           accent-sensitive
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
- Binary serialization formats (protobuf, MessagePack, BSON, CBOR, Apple Biome/SEGB,
  EXIF/MP4/ID3 metadata) store strings as plain UTF-8/UTF-16 bytes, so their text
  is already found by the raw scan; only their binary-encoded numbers and dates aren't.
- Not decoded (yet): encrypted archives and keychains, RAR, disk images (dmg),
  brotli, PDF text in fonts without a Unicode map, OCR of images.

## Development

```sh
cargo test                 # unit tests (matcher shapes, decoders, timestamps)
./scripts/selftest.sh      # end-to-end: 50 checks across formats, encodings, accents and dates
```
