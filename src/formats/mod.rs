//! The decoding engine.
//!
//! Every format is detected by content (magic bytes, never the extension) and
//! falls in one of a few classes:
//!
//! * **streams**: compressed data that decodes to a single child buffer
//! * **containers**: archives & structured files that yield named children
//! * **documents**: searched raw *and* through their decoded parts (PDF)
//! * **structured**: walked by the scanner itself (plist, SQLite)
//!
//! Adding a format = a detection rule here + a decoder in one of the
//! sub-modules + a line in [`children`].

pub mod apple;
pub mod archive;
pub mod compress;
pub mod leveldb;
pub mod text;
pub mod time;

use std::io::Read;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    // streams
    Gzip,
    Bzip2,
    Xz,
    Lzma,
    Zstd,
    Zlib,
    Lz4,
    AppleLz4,
    Lzfse,
    Snappy,
    // containers
    Zip,
    Tar,
    SevenZ,
    Cab,
    Cpio,
    Ole,
    LevelDb,
    TypedStream,
    // structured / documents
    BPlist,
    Sqlite,
    SqliteWal,
    Pdf,
    Other,
}

impl Format {
    pub fn label(self) -> &'static str {
        use Format::*;
        match self {
            Gzip => "gzip",
            Bzip2 => "bzip2",
            Xz => "xz",
            Lzma => "lzma",
            Zstd => "zstd",
            Zlib => "zlib",
            Lz4 => "lz4",
            AppleLz4 => "apple-lz4",
            Lzfse => "lzfse",
            Snappy => "snappy",
            Zip => "zip",
            Tar => "tar",
            SevenZ => "7z",
            Cab => "cab",
            Cpio => "cpio",
            Ole => "ole",
            LevelDb => "leveldb",
            TypedStream => "typedstream",
            BPlist => "bplist",
            Sqlite => "sqlite",
            SqliteWal => "sqlite-wal",
            Pdf => "pdf",
            Other => "",
        }
    }

    pub fn is_stream(self) -> bool {
        use Format::*;
        matches!(self, Gzip | Bzip2 | Xz | Lzma | Zstd | Zlib | Lz4 | AppleLz4 | Lzfse | Snappy)
    }

    pub fn is_container(self) -> bool {
        use Format::*;
        self.is_stream() || matches!(self, Zip | Tar | SevenZ | Cab | Cpio | Ole | LevelDb | TypedStream)
    }

    /// Archives show their entries in the live status panel.
    pub fn is_archive(self) -> bool {
        use Format::*;
        matches!(self, Zip | Tar | SevenZ | Cab | Cpio | Ole | LevelDb)
    }
}

pub fn detect(d: &[u8]) -> Format {
    use Format::*;
    let starts = |m: &[u8]| d.starts_with(m);
    if starts(&[0x1f, 0x8b]) {
        Gzip
    } else if starts(b"BZh") && d.len() > 4 && d[3].is_ascii_digit() {
        Bzip2
    } else if starts(&[0xFD, b'7', b'z', b'X', b'Z', 0]) {
        Xz
    } else if starts(&[0x28, 0xB5, 0x2F, 0xFD]) {
        Zstd
    } else if starts(&[0x04, 0x22, 0x4D, 0x18]) {
        Lz4
    } else if starts(b"bv41") || starts(b"bv4-") {
        AppleLz4
    } else if starts(b"bvx2") || starts(b"bvx1") || starts(b"bvxn") || starts(b"bvx-") {
        Lzfse
    } else if starts(&[0xFF, 0x06, 0x00, 0x00, b's', b'N', b'a', b'P', b'p', b'Y']) {
        Snappy
    } else if starts(b"PK\x03\x04") || starts(b"PK\x05\x06") {
        Zip
    } else if starts(&[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C]) {
        SevenZ
    } else if starts(b"MSCF\0\0\0\0") {
        Cab
    } else if starts(b"070701") || starts(b"070702") || starts(b"070707") {
        Cpio
    } else if starts(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) {
        Ole
    } else if starts(b"bplist0") {
        BPlist
    } else if starts(b"\x04\x0bstreamtyped") {
        TypedStream
    } else if starts(b"SQLite format 3\0") {
        Sqlite
    } else if d.len() >= 32 && (starts(&[0x37, 0x7f, 0x06, 0x82]) || starts(&[0x37, 0x7f, 0x06, 0x83])) {
        SqliteWal
    } else if starts(b"%PDF") {
        Pdf
    } else if d.len() > 262 && &d[257..262] == b"ustar" {
        Tar
    } else if leveldb::is_sstable(d) {
        LevelDb
    } else if d.len() > 13 && d[0] == 0x5D && d[1] == 0 && d[2] == 0 && (d[5..13] == [0xFF; 8] || d[12] == 0) {
        Lzma
    } else if d.len() > 2 && d[0] == 0x78 && matches!(d[1], 0x01 | 0x5E | 0x9C | 0xDA) {
        Zlib
    } else {
        Other
    }
}

/// One decoded piece of a container.
pub struct Child {
    /// Breadcrumb segment, e.g. `zip:word/document.xml`.
    pub label: String,
    /// Entry name to match against the needle, if the format has names.
    pub name: Option<String>,
    pub data: Vec<u8>,
}

/// Feed every child of a container to `f` (return `false` from `f` to stop).
/// Returns `false` if the data couldn't be opened as `fmt`, so the caller can
/// fall back to a raw scan.
pub fn children(fmt: Format, d: &[u8], cap: usize, f: &mut dyn FnMut(Child) -> bool) -> bool {
    use Format::*;
    if fmt.is_stream() {
        return match compress::decompress(fmt, d, cap) {
            Some(data) => {
                f(Child { label: fmt.label().to_string(), name: None, data });
                true
            }
            None => false,
        };
    }
    match fmt {
        Zip => archive::zip(d, cap, f),
        Tar => archive::tar(d, cap, f),
        SevenZ => archive::sevenz(d, cap, f),
        Cab => archive::cab(d, cap, f),
        Cpio => archive::cpio(d, cap, f),
        Ole => archive::ole(d, cap, f),
        LevelDb => leveldb::records(d, cap, f),
        TypedStream => apple::typedstream(d, f),
        _ => false,
    }
}

/// Read up to `cap` bytes; a truncated/corrupt stream still yields what decoded.
pub fn read_capped<R: Read>(mut r: R, cap: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    while out.len() < cap {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&buf[..n.min(cap - out.len())]),
            Err(_) => break,
        }
    }
    if out.is_empty() { None } else { Some(out) }
}
