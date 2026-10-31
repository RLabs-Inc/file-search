#!/usr/bin/env bash
# Builds a corpus that hides "phoenix protocol" in every supported format and
# checks that file-search finds each one.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release -q
T=$(mktemp -d)
mkdir -p "$T"/corpus/{.hidden,deep/er/still,mail,db,docs}
python3 scripts/mkcorpus.py "$T/corpus"
mkfifo "$T/corpus/a.fifo"                      # must be skipped, never block
command -v xattr >/dev/null && xattr -w com.test.note "xattr phoenix protocol" "$T/corpus/plain.txt" || true
./target/release/file-search "$T/corpus" -find "phoenix protocol" -o "$T/out" -q >/dev/null
fail=0
check() { if grep -qF -- "$2" "$T/out/hits.csv"; then printf '  ✓ %s\n' "$1"; else printf '  ✗ %s\n' "$1"; fail=1; fi; }
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
rm -rf "$T"
exit $fail
