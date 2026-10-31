//! Apple-specific encodings.
//!
//! `typedstream` (NSArchiver) is the legacy archive format used for the
//! `attributedBody` column of iMessage's chat.db, where modern macOS keeps the
//! actual message text. Strings are stored as `+` (C string) items preceded
//! by a length; we pull them out as clean children.

use super::Child;

pub fn typedstream(d: &[u8], f: &mut dyn FnMut(Child) -> bool) -> bool {
    let mut found = 0;
    let mut i = 1;
    while i + 2 < d.len() {
        if d[i] == b'+' && d[i - 1] == 0x01 {
            let (len, hdr) = match d[i + 1] {
                0x81 => (d.get(i + 2..i + 4).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize), 3),
                0x82 => (d.get(i + 2..i + 6).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize), 5),
                n if n < 0x80 => (Some(n as usize), 1),
                _ => (None, 0),
            };
            if let Some(len) = len {
                let start = i + 1 + hdr;
                if let Some(s) = d.get(start..start + len).and_then(|b| std::str::from_utf8(b).ok())
                    && len > 0 && s.chars().any(|c| !c.is_control()) {
                        found += 1;
                        let child = Child { label: format!("typedstream:string@0x{start:x}"), name: None, data: s.as_bytes().to_vec() };
                        if !f(child) {
                            return true;
                        }
                        i = start + len;
                        continue;
                    }
            }
        }
        i += 1;
    }
    found > 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imessage_attributed_body() {
        // shape of a real attributedBody blob, abbreviated
        let text = "Meet me at the café — phoenix protocol 🚀";
        let mut d = b"\x04\x0bstreamtyped\x81\xe8\x03\x84\x01@\x84\x84\x84\x12NSAttributedString\x00\x84\x84\x08NSObject\x00\x85\x92\x84\x84\x84\x08NSString\x01\x94\x84\x01+".to_vec();
        d.push(text.len() as u8);
        d.extend(text.as_bytes());
        d.extend(b"\x86\x84\x02iI\x01\x13\x92\x84\x84\x84\x0cNSDictionary\x00");
        let mut got = Vec::new();
        assert!(typedstream(&d, &mut |c| {
            got.push(String::from_utf8(c.data).unwrap());
            true
        }));
        assert_eq!(got, vec![text.to_string()]);
    }
}
