//! LevelDB / RocksDB SSTables (.ldb / .sst): Chrome, Edge, Electron apps
//! (Slack, Discord, VS Code, WhatsApp Desktop, Teams…) store Local Storage,
//! IndexedDB and more in them, with blocks usually Snappy-compressed.

use super::Child;

const MAGIC: [u8; 8] = 0xdb47_7524_8b80_fb57u64.to_le_bytes();

pub fn is_sstable(d: &[u8]) -> bool {
    d.len() >= 48 && d[d.len() - 8..] == MAGIC
}

fn varint(d: &[u8], p: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = *d.get(*p)?;
        *p += 1;
        v |= ((b & 0x7F) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

fn handle(d: &[u8], p: &mut usize) -> Option<(usize, usize)> {
    Some((varint(d, p)? as usize, varint(d, p)? as usize))
}

fn read_block(d: &[u8], off: usize, size: usize) -> Option<Vec<u8>> {
    let raw = d.get(off..off + size)?;
    match d.get(off + size)? {
        0 => Some(raw.to_vec()),
        1 => snap::raw::Decoder::new().decompress_vec(raw).ok(),
        // RocksDB: 4 = LZ4, 7 = ZSTD (both prefixed with the decompressed size varint)
        4 => {
            let mut p = 0;
            let n = varint(raw, &mut p)? as usize;
            lz4_flex::block::decompress(&raw[p..], n).ok()
        }
        7 => {
            let mut p = 0;
            varint(raw, &mut p)?;
            super::read_capped(ruzstd::decoding::StreamingDecoder::new(std::io::Cursor::new(&raw[p..])).ok()?, 256 << 20)
        }
        _ => None,
    }
}

/// (key, value) pairs of one block, resolving the shared-prefix key compression.
fn entries(b: &[u8]) -> Vec<(Vec<u8>, &[u8])> {
    let mut out = Vec::new();
    if b.len() < 4 {
        return out;
    }
    let restarts = u32::from_le_bytes(b[b.len() - 4..].try_into().unwrap()) as usize;
    let Some(end) = b.len().checked_sub(4 + restarts * 4) else { return out };
    let mut p = 0;
    let mut key: Vec<u8> = Vec::new();
    while p < end {
        let (Some(shared), Some(unshared), Some(vlen)) = (varint(b, &mut p), varint(b, &mut p), varint(b, &mut p)) else {
            break;
        };
        let (shared, unshared, vlen) = (shared as usize, unshared as usize, vlen as usize);
        if shared > key.len() || p + unshared + vlen > end {
            break;
        }
        key.truncate(shared);
        key.extend_from_slice(&b[p..p + unshared]);
        p += unshared;
        out.push((key.clone(), &b[p..p + vlen]));
        p += vlen;
    }
    out
}

fn printable_key(k: &[u8]) -> String {
    // internal keys end with an 8-byte sequence/type trailer
    let k = if k.len() > 8 { &k[..k.len() - 8] } else { k };
    let s: String = String::from_utf8_lossy(k).chars().map(|c| if c.is_control() { '·' } else { c }).collect();
    if s.chars().count() > 80 { format!("{}…", s.chars().take(79).collect::<String>()) } else { s }
}

pub fn records(d: &[u8], cap: usize, f: &mut dyn FnMut(Child) -> bool) -> bool {
    let footer = &d[d.len() - 48..];
    let mut p = 0;
    let (Some(_meta), Some((ioff, isize))) = (handle(footer, &mut p), handle(footer, &mut p)) else { return false };
    let Some(index) = read_block(d, ioff, isize) else { return false };
    let mut total = 0;
    for (_, h) in entries(&index) {
        let mut q = 0;
        let Some((off, size)) = handle(h, &mut q) else { continue };
        let Some(block) = read_block(d, off, size) else { continue };
        for (k, v) in entries(&block) {
            total += v.len();
            let key = printable_key(&k);
            let child = Child { label: format!("leveldb:{key}"), name: Some(key), data: v.to_vec() };
            if !f(child) || total > cap {
                return true;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_varint(v: &mut Vec<u8>, mut x: u64) {
        while x >= 0x80 {
            v.push((x as u8) | 0x80);
            x >>= 7;
        }
        v.push(x as u8);
    }

    fn block(kvs: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut b = Vec::new();
        let mut prev: &[u8] = b"";
        for (k, v) in kvs {
            let shared = prev.iter().zip(k.iter()).take_while(|(a, b)| a == b).count();
            put_varint(&mut b, shared as u64);
            put_varint(&mut b, (k.len() - shared) as u64);
            put_varint(&mut b, v.len() as u64);
            b.extend(&k[shared..]);
            b.extend(*v);
            prev = k;
        }
        b.extend(0u32.to_le_bytes()); // one restart at 0
        b.extend(1u32.to_le_bytes());
        b
    }

    #[test]
    fn sstable_with_snappy_blocks() {
        let mut file = Vec::new();
        let k1 = b"_https://web.example.com\x00\x01note\x01\x00\x00\x00\x00\x00\x00\x00";
        let k2 = b"_https://web.example.com\x00\x01other\x02\x00\x00\x00\x00\x00\x00\x00";
        let data = block(&[(k1, b"\x01the phoenix protocol is go"), (k2, b"\x01x")]);
        let comp = snap::raw::Encoder::new().compress_vec(&data).unwrap();
        let (doff, dsize) = (file.len(), comp.len());
        file.extend(&comp);
        file.push(1); // snappy
        file.extend([0u8; 4]); // crc (not checked)
        let mut hv = Vec::new();
        put_varint(&mut hv, doff as u64);
        put_varint(&mut hv, dsize as u64);
        let index = block(&[(k2, &hv)]);
        let (ioff, isize) = (file.len(), index.len());
        file.extend(&index);
        file.push(0);
        file.extend([0u8; 4]);
        let mut footer = Vec::new();
        put_varint(&mut footer, 0);
        put_varint(&mut footer, 0);
        put_varint(&mut footer, ioff as u64);
        put_varint(&mut footer, isize as u64);
        footer.resize(40, 0);
        footer.extend(MAGIC);
        file.extend(footer);

        assert!(is_sstable(&file));
        let mut got = Vec::new();
        assert!(records(&file, 1 << 20, &mut |c| {
            got.push((c.label, c.data));
            true
        }));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "leveldb:_https://web.example.com··note");
        assert!(got[0].1.ends_with(b"phoenix protocol is go"));
    }
}
