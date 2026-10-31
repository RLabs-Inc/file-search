import os, zipfile, tarfile, io, gzip, bz2, lzma, zlib, plistlib, sqlite3, base64, json, shutil, datetime, urllib.parse
N = "Phoenix Protocol"
os.chdir(os.sys.argv[1])
open("plain.txt","w").write("Line one\nThe phoenix\n   protocol was approved.\nlast line\n")
open(".hidden/.secret","w").write("hidden: PHOENIX PROTOCOL\n")
open("deep/er/still/utf16le.txt","wb").write("﻿notes about Phoenix Protocol here".encode("utf-16-le"))
open("deep/utf16be.bin","wb").write(b"\x00\x01"+"xx phoenix protocol yy".encode("utf-16-be"))
# docx-like with split runs
z = zipfile.ZipFile("docs/report.docx","w",zipfile.ZIP_DEFLATED)
z.writestr("word/document.xml", '<?xml version="1.0"?><w:document><w:body><w:p><w:r><w:t>The Phoe</w:t></w:r><w:r><w:t>nix Pro</w:t></w:r><w:r><w:t>tocol &amp; more</w:t></w:r></w:p></w:body></w:document>')
z.writestr("docProps/core.xml","<cp:coreProperties><dc:title>quarterly</dc:title></cp:coreProperties>")
z.close()
# nested: tar.gz containing a zip containing text
bio = io.BytesIO(); zz = zipfile.ZipFile(bio,"w",zipfile.ZIP_DEFLATED); zz.writestr("inner/readme.md","# phoenix protocol\nnested deep"); zz.close()
with tarfile.open("deep/bundle.tar.gz","w:gz") as t:
    ti = tarfile.TarInfo("pkg/inner.zip"); ti.size=len(bio.getvalue()); t.addfile(ti, io.BytesIO(bio.getvalue()))
open("deep/log.bz2","wb").write(bz2.compress(b"x"*1000+b" phoenix protocol "+b"y"*1000))
open("deep/log.xz","wb").write(lzma.compress(b"start phoenix protocol end"))
open("deep/blob.zlib","wb").write(zlib.compress(b"zlib says: Phoenix Protocol!"))
# binary plist with nested bplist data + date
inner = plistlib.dumps({"note":"the phoenix protocol lives here"}, fmt=plistlib.FMT_BINARY)
pl = {"name":"x","payload":inner,"when":datetime.datetime(2024,3,15,10,0,0),"items":["a","phoenix protocol in array"]}
open("deep/settings.plist","wb").write(plistlib.dumps(pl, fmt=plistlib.FMT_BINARY))
open("deep/settings.xml.plist","wb").write(plistlib.dumps({"payload":plistlib.dumps({"k":"xml-embedded phoenix protocol"*3})}))
# sqlite: live row, deleted row, timestamp, gzip blob
db = sqlite3.connect("db/chat.db"); db.execute("pragma secure_delete=0")
db.execute("create table message(id integer primary key, text text, date integer, attachment blob, phone text)")
db.execute("insert into message(text,date,attachment,phone) values (?,?,?,?)", ("we need the phoenix protocol now", 1710496800, gzip.compress(b"blob: phoenix protocol"), "(11) 98765-4321"))
db.execute("insert into message(text,date,phone) values (?,?,?)", ("DELETED: phoenix protocol leaked", 0, ""))
db.execute("insert into message(text,date) values (?,?)", ("other", 732153600))
db.commit(); db.execute("delete from message where text like 'DELETED%'"); db.commit(); db.close()
# WAL db: copy the -wal while still open
w = sqlite3.connect("db/wal.db"); w.execute("pragma journal_mode=wal"); w.execute("create table t(x text)"); w.commit()
w.execute("insert into t values ('wal only phoenix protocol')"); w.commit()
shutil.copy("db/wal.db-wal","db/copy.db-wal"); w.close()
# email: base64 + quoted-printable
b64 = base64.encodebytes(("Hello,\nplease review the Phoenix Protocol attached.\n"*3).encode()).decode()
open("mail/msg.eml","w").write(f"""From: a@b.c\nSubject: test\nContent-Type: multipart/mixed; boundary=X\n\n--X\nContent-Transfer-Encoding: base64\n\n{b64}\n--X\nContent-Transfer-Encoding: quoted-printable\n\nThe Phoe=\nnix Proto=63ol in QP caf=C3=A9\n--X--\n""")
open("data.json","w").write(json.dumps({"msg":"Phönix", "real":"Phoenix Protocol"}).replace("Phoenix Protocol","Ph\\u006fenix Protocol"))
open("access.log","w").write("GET /search?q="+urllib.parse.quote("phoenix protocol")+" 200\n")
open("dump.hex","w").write(" ".join(f"{b:02x}" for b in b"Phoenix Protocol")+"\n")
open("short.b64","w").write(base64.b64encode(b"xPhoenix Protocol").decode()+"\n")
open("raw.bin","wb").write(os.urandom(5000)+b"Phoenix Protocol"+os.urandom(5000))
open("phoenix-protocol-notes.txt","w").write("nothing inside\n")
os.makedirs("Phoenix Protocol folder", exist_ok=True)
# pdf with flate stream + TJ kerning
content = zlib.compress(b"BT /F1 12 Tf [(Phoe) -20 (nix) -300 (Protocol)] TJ ET")
open("docs/paper.pdf","wb").write(b"%%PDF-1.4\n1 0 obj\n<< /Length %d /Filter /FlateDecode >>\nstream\n" % len(content) + content + b"\nendstream\nendobj\n%%EOF\n")
open("contacts.txt","w").write("Ana: +55 11 98765-4321\nBob: 11.98765.4321\nmeeting 15/03/2024, also March 15, 2024\n")
