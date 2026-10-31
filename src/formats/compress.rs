//! Compressed streams, plus carving of gzip streams embedded in binaries
//! (browser caches, app blobs, firmware…).

use std::io::Cursor;

use super::{read_capped, Format};

pub fn decompress(fmt: Format, d: &[u8], cap: usize) -> Option<Vec<u8>> {
    match fmt {
        Format::Gzip => read_capped(flate2::read::MultiGzDecoder::new(d), cap),
        Format::Zlib => read_capped(flate2::read::ZlibDecoder::new(d), cap),
        Format::Bzip2 => read_capped(bzip2::read::MultiBzDecoder::new(d), cap),
        Format::Zstd => read_capped(ruzstd::decoding::StreamingDecoder::new(Cursor::new(d)).ok()?, cap),
        Format::Lz4 => read_capped(lz4_flex::frame::FrameDecoder::new(d), cap),
        Format::Snappy => read_capped(snap::read::FrameDecoder::new(d), cap),
        Format::AppleLz4 => apple_lz4(d, cap),
        Format::Lzfse => {
            let mut out = Vec::new();
            // a damaged stream may still have produced useful output
            let _ = lzfse_rust::decode_bytes(d, &mut out);
            out.truncate(cap);
            if out.is_empty() { None } else { Some(out) }
        }
        Format::Xz | Format::Lzma => {
            let mut w = CapWriter { buf: Vec::new(), cap };
            let mut input = std::io::BufReader::new(d);
            let _ = if fmt == Format::Xz {
                lzma_rs::xz_decompress(&mut input, &mut w)
            } else {
                lzma_rs::lzma_decompress(&mut input, &mut w)
            };
            if w.buf.is_empty() { None } else { Some(w.buf) }
        }
        _ => None,
    }
}

/// Apple's compression_lz4 framing: `bv41` blocks (and raw `bv4-`) ending with `bv4$`.
fn apple_lz4(d: &[u8], cap: usize) -> Option<Vec<u8>> {
    let u32le = |p: usize| d.get(p..p + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let mut out = Vec::new();
    let mut p = 0;
    while p + 4 <= d.len() && out.len() < cap {
        match &d[p..p + 4] {
            b"bv41" => {
                let (raw, comp) = (u32le(p + 4)?, u32le(p + 8)?);
                let block = d.get(p + 12..p + 12 + comp)?;
                match lz4_flex::block::decompress(block, raw) {
                    Ok(b) => out.extend_from_slice(&b),
                    Err(_) => break,
                }
                p += 12 + comp;
            }
            b"bv4-" => {
                let raw = u32le(p + 4)?;
                out.extend_from_slice(d.get(p + 8..p + 8 + raw)?);
                p += 8 + raw;
            }
            _ => break, // bv4$ or garbage
        }
    }
    out.truncate(cap);
    if out.is_empty() { None } else { Some(out) }
}

struct CapWriter {
    buf: Vec<u8>,
    cap: usize,
}

impl std::io::Write for CapWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() >= self.cap {
            return Err(std::io::Error::other("cap reached"));
        }
        let n = data.len().min(self.cap - self.buf.len());
        self.buf.extend_from_slice(&data[..n]);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Find gzip streams that start somewhere *inside* a buffer and inflate them.
pub fn carve_gzip(d: &[u8], cap: usize, max: usize) -> Vec<(usize, Vec<u8>)> {
    let mut out = Vec::new();
    let mut total = 0;
    let mut i = 1;
    while i + 10 < d.len() && out.len() < max && total < cap {
        let Some(p) = d[i..].windows(3).position(|w| w == [0x1f, 0x8b, 0x08]) else { break };
        let at = i + p;
        // flags: reserved bits must be zero; OS byte is a known value
        if d[at + 3] & 0xE0 == 0 && (d[at + 9] <= 13 || d[at + 9] == 255)
            && let Some(dec) = read_capped(flate2::read::GzDecoder::new(&d[at..]), cap - total) {
                total += dec.len();
                out.push((at, dec));
            }
        i = at + 3;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &[u8] = b"hello from inside a compressed stream: phoenix protocol";

    #[test]
    fn lz4_frame() {
        let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
        std::io::Write::write_all(&mut enc, TEXT).unwrap();
        let data = enc.finish().unwrap();
        assert_eq!(super::super::detect(&data), Format::Lz4);
        assert_eq!(decompress(Format::Lz4, &data, 1 << 20).unwrap(), TEXT);
    }

    #[test]
    fn apple_lz4_blocks() {
        let block = lz4_flex::block::compress(TEXT);
        let mut data = b"bv41".to_vec();
        data.extend((TEXT.len() as u32).to_le_bytes());
        data.extend((block.len() as u32).to_le_bytes());
        data.extend(&block);
        data.extend(b"bv4$");
        assert_eq!(decompress(Format::AppleLz4, &data, 1 << 20).unwrap(), TEXT);
    }

    #[test]
    fn lzfse() {
        let mut data = Vec::new();
        lzfse_rust::encode_bytes(TEXT, &mut data).unwrap();
        assert_eq!(super::super::detect(&data), Format::Lzfse);
        assert_eq!(decompress(Format::Lzfse, &data, 1 << 20).unwrap(), TEXT);
    }

    #[test]
    fn snappy_framed() {
        let mut enc = snap::write::FrameEncoder::new(Vec::new());
        std::io::Write::write_all(&mut enc, TEXT).unwrap();
        let data = enc.into_inner().unwrap();
        assert_eq!(super::super::detect(&data), Format::Snappy);
        assert_eq!(decompress(Format::Snappy, &data, 1 << 20).unwrap(), TEXT);
    }

    #[test]
    fn carving() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, TEXT).unwrap();
        let mut data = vec![0xAAu8; 300];
        data.extend(gz.finish().unwrap());
        data.extend([0x55u8; 300]);
        let c = carve_gzip(&data, 1 << 20, 10);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, 300);
        assert_eq!(c[0].1, TEXT);
    }
}
