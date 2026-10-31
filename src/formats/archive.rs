//! Archives and compound files: zip (Office, epub, jar…), tar, 7z, cab,
//! cpio, OLE/CFB (legacy Office, Outlook .msg, Thumbs.db), PDF streams.

use std::io::Cursor;
use std::sync::LazyLock;

use regex::bytes::Regex;

use super::{read_capped, Child};

type Sink<'a> = &'a mut dyn FnMut(Child) -> bool;

fn entry(prefix: &str, name: String, data: Vec<u8>) -> Child {
    Child { label: format!("{prefix}:{name}"), name: Some(name), data }
}

pub fn zip(d: &[u8], cap: usize, f: Sink) -> bool {
    let Ok(mut z) = ::zip::ZipArchive::new(Cursor::new(d)) else { return false };
    for i in 0..z.len() {
        let Ok(mut e) = z.by_index(i) else { continue };
        if e.is_dir() {
            continue;
        }
        let name = e.name().map(|c| c.to_string()).unwrap_or_else(|_| format!("#{i}"));
        let data = read_capped(&mut e, cap).unwrap_or_default();
        if !f(entry("zip", name, data)) {
            break;
        }
    }
    true
}

pub fn tar(d: &[u8], cap: usize, f: Sink) -> bool {
    let mut ar = ::tar::Archive::new(Cursor::new(d));
    let Ok(entries) = ar.entries() else { return false };
    for e in entries {
        let Ok(mut e) = e else { break };
        if !e.header().entry_type().is_file() {
            continue;
        }
        let name = e.path().map(|p| p.display().to_string()).unwrap_or_default();
        let data = read_capped(&mut e, cap).unwrap_or_default();
        if !f(entry("tar", name, data)) {
            break;
        }
    }
    true
}

pub fn sevenz(d: &[u8], cap: usize, f: Sink) -> bool {
    let Ok(mut ar) = sevenz_rust2::ArchiveReader::new(Cursor::new(d), sevenz_rust2::Password::empty()) else {
        return false;
    };
    let _ = ar.for_each_entries(|e, r| {
        if e.is_directory() {
            return Ok(true);
        }
        let data = read_capped(r, cap).unwrap_or_default();
        Ok(f(entry("7z", e.name().to_string(), data)))
    });
    true
}

pub fn cab(d: &[u8], cap: usize, f: Sink) -> bool {
    let Ok(mut c) = ::cab::Cabinet::new(Cursor::new(d)) else { return false };
    let names: Vec<String> =
        c.folder_entries().flat_map(|fo| fo.file_entries().map(|fi| fi.name().to_string()).collect::<Vec<_>>()).collect();
    for name in names {
        let data = match c.read_file(&name) {
            Ok(r) => read_capped(r, cap).unwrap_or_default(),
            Err(_) => continue,
        };
        if !f(entry("cab", name, data)) {
            break;
        }
    }
    true
}

/// cpio "newc"/"crc" (ASCII hex) and "odc" (ASCII octal) archives.
pub fn cpio(d: &[u8], cap: usize, f: Sink) -> bool {
    let field = |s: &[u8], radix: u32| std::str::from_utf8(s).ok().and_then(|t| usize::from_str_radix(t, radix).ok());
    let align4 = |x: usize| (x + 3) & !3;
    let mut p = 0;
    let mut any = false;
    while p + 76 <= d.len() {
        let (name_at, namesize, filesize, newc) = match &d[p..p + 6] {
            b"070701" | b"070702" if p + 110 <= d.len() => {
                let fs = field(&d[p + 54..p + 62], 16);
                let ns = field(&d[p + 94..p + 102], 16);
                match (ns, fs) {
                    (Some(n), Some(s)) => (p + 110, n, s, true),
                    _ => break,
                }
            }
            b"070707" => match (field(&d[p + 59..p + 65], 8), field(&d[p + 65..p + 76], 8)) {
                (Some(n), Some(s)) => (p + 76, n, s, false),
                _ => break,
            },
            _ => break,
        };
        let Some(raw_name) = d.get(name_at..name_at + namesize) else { break };
        let name = String::from_utf8_lossy(raw_name.strip_suffix(&[0]).unwrap_or(raw_name)).to_string();
        if name == "TRAILER!!!" {
            break;
        }
        let data_at = if newc { align4(name_at + namesize) } else { name_at + namesize };
        let Some(data) = d.get(data_at..data_at + filesize) else { break };
        any = true;
        if filesize > 0 && !f(entry("cpio", name, data[..data.len().min(cap)].to_vec())) {
            break;
        }
        p = if newc { align4(data_at + filesize) } else { data_at + filesize };
    }
    any
}

/// OLE2 / Compound File Binary (legacy .doc/.xls/.ppt, Outlook .msg,
/// Thumbs.db, Sticky Notes…): every stream, reassembled from its sectors.
///
/// Our own lenient reader: real-world files (even ones written by Apple's
/// textutil) often break a strict reader's consistency checks, yet their
/// streams are perfectly readable.
pub fn ole(d: &[u8], cap: usize, f: Sink) -> bool {
    let Some(cf) = Ole::open(d) else { return false };
    let mut streams = Vec::new();
    cf.collect(cf.entry(0).map(|e| e.child).unwrap_or(NONE), "", &mut streams, &mut vec![false; cf.entries()], 0);
    if streams.is_empty() {
        return false;
    }
    for (path, start, size) in streams {
        let data = cf.stream(start, size.min(cap as u64) as usize);
        if !f(entry("ole", path, data)) {
            break;
        }
    }
    true
}

const NONE: u32 = 0xFFFF_FFFF;
const END: u32 = 0xFFFF_FFFE;

struct DirEntry {
    name: String,
    kind: u8,
    left: u32,
    right: u32,
    child: u32,
    start: u32,
    size: u64,
}

struct Ole<'a> {
    d: &'a [u8],
    ss: usize,
    mini_ss: usize,
    cutoff: u64,
    fat: Vec<u32>,
    minifat: Vec<u32>,
    dir: Vec<u8>,
    mini_stream: Vec<u8>,
}

impl<'a> Ole<'a> {
    fn u16(d: &[u8], o: usize) -> u16 {
        d.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0)
    }
    fn u32(d: &[u8], o: usize) -> u32 {
        d.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(NONE)
    }

    fn sector(&self, n: u32) -> Option<&'a [u8]> {
        let o = (n as usize + 1).checked_mul(self.ss)?;
        self.d.get(o..o + self.ss)
    }

    /// Follow a FAT chain, guarding against loops and runaway lengths.
    fn chain(table: &[u32], start: u32, limit: usize) -> Vec<u32> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cur = start;
        while (cur as usize) < table.len().max(1) && cur < END && out.len() < limit && seen.insert(cur) {
            out.push(cur);
            cur = *table.get(cur as usize).unwrap_or(&END);
        }
        out
    }

    fn open(d: &'a [u8]) -> Option<Ole<'a>> {
        if d.len() < 512 {
            return None;
        }
        let ss = 1usize << Self::u16(d, 0x1E).clamp(7, 16);
        let mini_ss = 1usize << Self::u16(d, 0x20).clamp(2, 12);
        let mut ole = Ole { d, ss, mini_ss, cutoff: Self::u32(d, 0x38) as u64, fat: Vec::new(), minifat: Vec::new(), dir: Vec::new(), mini_stream: Vec::new() };
        // FAT sector list: 109 in the header, the rest in the DIFAT chain
        let mut fat_sectors: Vec<u32> = (0..109).map(|i| Self::u32(d, 0x4C + i * 4)).filter(|&s| s < END).collect();
        let mut difat = Self::u32(d, 0x44);
        let mut guard = 0;
        while difat < END && guard < 100_000 {
            let Some(sec) = ole.sector(difat) else { break };
            let n = ss / 4 - 1;
            fat_sectors.extend((0..n).map(|i| Self::u32(sec, i * 4)).filter(|&s| s < END));
            difat = Self::u32(sec, n * 4);
            guard += 1;
        }
        for s in fat_sectors {
            if let Some(sec) = ole.sector(s) {
                ole.fat.extend((0..ss / 4).map(|i| Self::u32(sec, i * 4)));
            }
        }
        if ole.fat.is_empty() {
            return None;
        }
        let limit = d.len() / ss + 1;
        ole.dir = ole.read_chain(Self::u32(d, 0x30), limit);
        let mf = ole.read_chain(Self::u32(d, 0x3C), limit);
        ole.minifat = (0..mf.len() / 4).map(|i| Self::u32(&mf, i * 4)).collect();
        if let Some(root) = ole.entry(0) {
            let mut ms = ole.read_chain(root.start, limit);
            ms.truncate(root.size as usize);
            ole.mini_stream = ms;
        }
        Some(ole)
    }

    fn read_chain(&self, start: u32, limit: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for s in Self::chain(&self.fat, start, limit) {
            match self.sector(s) {
                Some(sec) => out.extend_from_slice(sec),
                None => break,
            }
        }
        out
    }

    fn entries(&self) -> usize {
        self.dir.len() / 128
    }

    fn entry(&self, i: u32) -> Option<DirEntry> {
        let e = self.dir.get(i as usize * 128..i as usize * 128 + 128)?;
        let nlen = (Self::u16(e, 0x40) as usize).min(64);
        let units: Vec<u16> = (0..nlen / 2).map(|k| Self::u16(e, k * 2)).take_while(|&u| u != 0).collect();
        Some(DirEntry {
            name: String::from_utf16_lossy(&units).chars().filter(|c| !c.is_control()).collect(),
            kind: e[0x42],
            left: Self::u32(e, 0x44),
            right: Self::u32(e, 0x48),
            child: Self::u32(e, 0x4C),
            start: Self::u32(e, 0x74),
            size: Self::u32(e, 0x78) as u64,
        })
    }

    /// Walk the red-black sibling tree + children, collecting stream paths.
    fn collect(&self, i: u32, prefix: &str, out: &mut Vec<(String, u32, u64)>, seen: &mut Vec<bool>, depth: usize) {
        if i == NONE || depth > 64 || (i as usize) >= seen.len() || seen[i as usize] {
            return;
        }
        seen[i as usize] = true;
        let Some(e) = self.entry(i) else { return };
        self.collect(e.left, prefix, out, seen, depth + 1);
        let path = format!("{prefix}/{}", e.name);
        match e.kind {
            2 => out.push((path.clone(), e.start, e.size)),
            1 => self.collect(e.child, &path, out, seen, depth + 1),
            _ => {}
        }
        self.collect(e.right, prefix, out, seen, depth + 1);
    }

    fn stream(&self, start: u32, size: usize) -> Vec<u8> {
        let mut out = if (size as u64) < self.cutoff {
            let mut v = Vec::new();
            for s in Self::chain(&self.minifat, start, self.mini_stream.len() / self.mini_ss + 1) {
                let o = s as usize * self.mini_ss;
                match self.mini_stream.get(o..o + self.mini_ss) {
                    Some(b) => v.extend_from_slice(b),
                    None => break,
                }
            }
            v
        } else {
            self.read_chain(start, size / self.ss + 2)
        };
        out.truncate(size);
        out
    }
}

/// Inflate every FlateDecode stream in a PDF. Returns (offset, decoded).
pub fn pdf_streams(d: &[u8], cap: usize, max: usize) -> Vec<(usize, Vec<u8>)> {
    static RE_STREAM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?-u)stream\r?\n").unwrap());
    let mut out = Vec::new();
    let mut total = 0;
    for m in RE_STREAM.find_iter(d) {
        if out.len() >= max || total >= cap {
            break;
        }
        let dict = &d[m.start().saturating_sub(512)..m.start()];
        if !dict.windows(11).any(|w| w == b"FlateDecode") {
            continue;
        }
        let body = &d[m.end()..];
        let body = match body.windows(9).position(|w| w == b"endstream") {
            Some(p) => &body[..p],
            None => body,
        };
        if let Some(dec) = read_capped(flate2::read::ZlibDecoder::new(body), cap - total) {
            total += dec.len();
            out.push((m.end(), dec));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(r: impl Fn(&mut dyn FnMut(Child) -> bool) -> bool) -> Vec<(String, Vec<u8>)> {
        let mut v = Vec::new();
        assert!(r(&mut |c: Child| {
            v.push((c.label, c.data));
            true
        }));
        v
    }

    #[test]
    fn cpio_newc() {
        let mut d = Vec::new();
        for (name, body) in [("a.txt", &b"phoenix protocol"[..]), ("TRAILER!!!", &b""[..])] {
            let n = name.len() + 1;
            d.extend(format!("070701{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}{:08X}", 1, 0o100644, 0, 0, 1, 0, body.len(), 0, 0, 0, 0, n, 0).as_bytes());
            d.extend(name.as_bytes());
            d.push(0);
            while d.len() % 4 != 0 {
                d.push(0);
            }
            d.extend(body);
            while d.len() % 4 != 0 {
                d.push(0);
            }
        }
        let v = collect(|f| cpio(&d, 1 << 20, f));
        assert_eq!(v, vec![("cpio:a.txt".to_string(), b"phoenix protocol".to_vec())]);
    }

    #[test]
    fn cab_roundtrip() {
        let mut b = ::cab::CabinetBuilder::new();
        b.add_folder(::cab::CompressionType::MsZip).add_file("notes.txt");
        let mut w = b.build(Cursor::new(Vec::new())).unwrap();
        while let Some(mut fw) = w.next_file().unwrap() {
            std::io::Write::write_all(&mut fw, b"cab says phoenix protocol").unwrap();
        }
        let data = w.finish().unwrap().into_inner();
        assert_eq!(super::super::detect(&data), super::super::Format::Cab);
        let v = collect(|f| cab(&data, 1 << 20, f));
        assert_eq!(v[0].0, "cab:notes.txt");
        assert_eq!(v[0].1, b"cab says phoenix protocol");
    }

    #[test]
    fn ole_streams() {
        let mut cf = ::cfb::CompoundFile::create(Cursor::new(Vec::new())).unwrap();
        cf.create_storage("/Inner").unwrap();
        {
            let mut s = cf.create_stream("/Inner/Body").unwrap();
            std::io::Write::write_all(&mut s, &[b'x'; 5000]).unwrap(); // spans many sectors
            std::io::Write::write_all(&mut s, b" phoenix protocol").unwrap();
        }
        {
            let mut s = cf.create_stream("/Small").unwrap(); // < 4096 bytes: lives in the mini stream
            std::io::Write::write_all(&mut s, b"tiny phoenix protocol").unwrap();
        }
        let data = cf.into_inner().into_inner();
        assert_eq!(super::super::detect(&data), super::super::Format::Ole);
        let v = collect(|f| ole(&data, 1 << 20, f));
        assert!(v.iter().any(|(l, d)| l == "ole:/Inner/Body" && d.len() == 5017 && d.ends_with(b"phoenix protocol")), "{:?}", v.iter().map(|x| (&x.0, x.1.len())).collect::<Vec<_>>());
        assert!(v.iter().any(|(l, d)| l == "ole:/Small" && d == b"tiny phoenix protocol"));
    }
}

