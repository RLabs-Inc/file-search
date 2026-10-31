#!/usr/bin/env bash
# Builds a corpus that hides "phoenix protocol" in every supported format and
# checks that file-search finds each one.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q
T=$(mktemp -d)
mkdir -p "$T"/corpus/{.hidden,deep/er/still,mail,db,docs}
python3 scripts/mkcorpus.py "$T/corpus"
scripts/mkcorpus2.sh "$T/corpus" >/dev/null 2>&1
mkfifo "$T/corpus/a.fifo"                      # must be skipped, never block
command -v xattr >/dev/null && xattr -w com.test.note "xattr phoenix protocol" "$T/corpus/plain.txt" || true
FS=./target/release/file-search
$FS "$T/corpus" -find "phoenix protocol" -o "$T/out" -q >/dev/null
$FS "$T/corpus" -find "São Paulo" -o "$T/out-sp" -q >/dev/null
$FS "$T/corpus" -find "2024-03-15" -o "$T/out-date" -q >/dev/null
$FS "$T/corpus" -find "Привет мир" -o "$T/out-ru" -q >/dev/null
$FS "$T/corpus" -find "東京タワー" -o "$T/out-jp" -q >/dev/null
fail=0
OUT="$T/out"
check() { if grep -qF -- "$2" "$OUT/hits.csv"; then printf '  ✓ %s\n' "$1"; else printf '  ✗ %s\n' "$1"; fail=1; fi; }
echo "phoenix protocol:"
check "plain text, re-wrapped"        "plain.txt,,text"
check "hidden file"                   ".hidden/.secret"
check "file name"                     "file name"
check "folder name"                   "folder name"
check "UTF-16LE"                      "utf-16le"
check "UTF-16BE"                      "utf-16be"
check "docx (zip + split xml runs)"   "zip:word/document.xml ⟫ markup-stripped"
check "tar.gz > zip > file"           "gzip ⟫ tar:pkg/inner.zip ⟫ zip:inner/readme.md"
check "bzip2"                         ",bzip2,"
check "xz"                            ",xz,"
check "zlib"                          ",zlib,"
check "binary plist"                  "plist:/items/[1]"
check "nested bplist in data"         "plist:/payload ⟫ plist:/note"
check "base64 in xml plist"           "settings.xml.plist,base64@"
check "sqlite cell"                   "sqlite:message[rowid 1].text"
check "gzip blob in sqlite"           "sqlite:message[rowid 1].attachment ⟫ gzip"
check "deleted sqlite record"         "outside live rows"
check "WAL frame"                     "WAL frame"
check "MIME base64 body"              "msg.eml,base64@"
check "quoted-printable"              "quoted-printable"
check "JSON \\u escapes"              "unescaped"
check "URL-encoded"                   "url-decoded"
check "hex dump"                      "hex-text"
check "short base64"                  "decodes to"
check "raw binary"                    "raw.bin"
check "PDF FlateDecode + TJ"          "pdf-text"
check "extended attribute"            "xattr:com.test.note"
check "Apple LZFSE (compression_tool)" ",lzfse,"
check "Apple LZ4 (compression_tool)"  ",apple-lz4,"
check "LZ4 frame"                     ",lz4,"
check "7z archive"                    "7z:in7.txt"
check "cpio archive"                  "cpio:f.txt"
check "Word 97 .doc (OLE)"            "ole:/WordDocument"
check "RTF"                           "rtf-text"
check "gzip carved from binary"       "gzip@0x309"
check "MIME encoded-word header"      "mime-header"
check "iMessage typedstream"          "typedstream:string"
check "accent-insensitive (Protocól)" "latin1.txt"
echo "São Paulo:"; OUT="$T/out-sp"
check "plain + no accent + uppercase" "accents.txt"
check "macOS NFD file name"           "nome NFD.txt,,file name"
check "Latin-1 / cp1252"              ",cp1252,"
check "Mac Roman"                     ",mac-roman,"
echo "2024-03-15:"; OUT="$T/out-date"
check "unix seconds in JSON"          "⟨unix 2024-03-15"
check "unix ms in JSON"               "⟨unix-ms 2024-03-15"
check "Chrome/WebKit µs"              "⟨webkit 2024-03-15"
check "Mac ns in sqlite (iMessage)"   "mac-ns 2024-03-15"
check "EXIF date"                     "exif.txt"
check "bplist date"                   "plist:/when"
echo "Привет мир:"; OUT="$T/out-ru"
check "Cyrillic cp1251"               ",windows-1251,"
echo "東京タワー:"; OUT="$T/out-jp"
check "Japanese Shift_JIS"            ",Shift_JIS,"
rm -rf "$T"
exit $fail
