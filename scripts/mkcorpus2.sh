#!/usr/bin/env bash
# Extra fixtures that need real-world tools (macOS / Homebrew); each is skipped if its tool is missing.
set -uo pipefail
C="$1"; mkdir -p "$C/v2"; cd "$C/v2"
have() { command -v "$1" >/dev/null; }
echo "apple lzfse says phoenix protocol" > t1; echo "apple lz4 says phoenix protocol" > t2
have compression_tool && compression_tool -encode -a lzfse -i t1 -o msg.lzfse && compression_tool -encode -a lz4 -i t2 -o msg.applelz4
have lz4 && echo "lz4 frame phoenix protocol" | lz4 -q > msg.lz4
have 7z && { echo "inside 7z: phoenix protocol" > in7.txt; 7z a -bso0 -bsp0 arch.7z in7.txt; rm in7.txt; }
have cpio && { mkdir -p cp && echo "cpio phoenix protocol" > cp/f.txt && (cd cp && echo f.txt | cpio -o -H newc 2>/dev/null) > arch.cpio; rm -rf cp; }
have textutil && { echo "Word 97 document mentions the Phoenix Protocol." > w.txt; textutil -convert doc w.txt -output legacy.doc >/dev/null; \
                   printf '{\\rtf1\\ansi The Phoe\\b nix\\b0  Protocol in rtf}' > note.rtf; rm w.txt; }
rm -f t1 t2
python3 - <<'PY'
import os, gzip, sqlite3, unicodedata
# gzip stream hidden at an odd offset inside a binary blob
open("cache.bin","wb").write(os.urandom(777) + gzip.compress(b"carved: phoenix protocol") + os.urandom(333))
# legacy encodings + accents
open("latin1.txt","wb").write("arquivo em latin-1: Phoenix Protocól, cidade de São Paulo".encode("latin-1"))
open("macroman.txt","wb").write("mac roman: São Paulo".encode("mac_roman"))
open("cp1251.txt","wb").write("кириллица: Привет мир".encode("cp1251"))
open("sjis.txt","wb").write("日本語: 東京タワー".encode("shift_jis"))
open("accents.txt","w").write("sem acento: Sao Paulo / caixa: SÃO PAULO\n")
open(unicodedata.normalize("NFD","São Paulo — nome NFD.txt"),"w").write("x")
# MIME encoded-word header
open("hdr.eml","w").write("Subject: =?UTF-8?B?UGhvZW5peCBQcm90b2NvbA==?=\n\nbody\n")
# iMessage-style attributedBody (typedstream) in sqlite
text = "imessage body: phoenix protocol"
blob = b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01@\x84\x84\x84\x12NSAttributedString\x00\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x08NSString\x01\x94\x84\x01+" + bytes([len(text)]) + text.encode() + b"\x86\x84\x02iI\x01\x13\x92"
db = sqlite3.connect("imessage.db"); db.execute("create table message(text text, attributedBody blob, date integer)")
db.execute("insert into message values (NULL, ?, ?)", (blob, 732196800 * 10**9)); db.commit(); db.close()
# timestamps / EXIF-style dates for 2024-03-15
open("events.json","w").write('{"created": 1710496800, "ms": 1710496800123, "chrome": 13354970400000000}\n')
open("exif.txt","w").write("DateTimeOriginal: 2024:03:15 10:00:00\n")
PY
