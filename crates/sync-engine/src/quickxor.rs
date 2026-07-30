//! Microsoft QuickXorHash — the checksum OneDrive reports for file content
//! (`quickXorHash` in DriveItem hashes).
//!
//! Algorithm: a 160-bit accumulator; each input byte is XORed in at a bit
//! position that advances by 11 bits per byte (mod 160). The total length is
//! XORed into the trailing 8 bytes, and the digest is base64-encoded.
//! Reference: Microsoft's published C# implementation.

use base64::Engine;

const WIDTH_BITS: usize = 160;
const WIDTH_BYTES: usize = WIDTH_BITS / 8; // 20
const SHIFT: usize = 11;

pub struct QuickXorHash {
    data: [u8; WIDTH_BYTES],
    length: u64,
}

impl QuickXorHash {
    pub fn new() -> Self {
        Self {
            data: [0u8; WIDTH_BYTES],
            length: 0,
        }
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            let bit_pos = (self.length as usize).wrapping_mul(SHIFT) % WIDTH_BITS;
            let byte_pos = bit_pos / 8;
            let bit_off = bit_pos % 8;
            // Spread the byte across (up to) two accumulator cells.
            let v = (b as u16) << bit_off;
            self.data[byte_pos] ^= v as u8;
            self.data[(byte_pos + 1) % WIDTH_BYTES] ^= (v >> 8) as u8;
            self.length += 1;
        }
    }

    pub fn finalize(mut self) -> String {
        // XOR the little-endian length into the last 8 bytes.
        let len_bytes = self.length.to_le_bytes();
        for (i, b) in len_bytes.iter().enumerate() {
            self.data[WIDTH_BYTES - 8 + i] ^= b;
        }
        base64::engine::general_purpose::STANDARD.encode(self.data)
    }

    /// Hash a whole byte slice in one call.
    pub fn hash(bytes: &[u8]) -> String {
        let mut h = Self::new();
        h.update(bytes);
        h.finalize()
    }

    /// Hash a file by streaming it in 64 KiB chunks (blocking I/O — call from
    /// a blocking context).
    pub fn hash_file(path: &std::path::Path) -> std::io::Result<String> {
        use std::io::Read;
        let mut file = std::fs::File::open(path)?;
        let mut h = Self::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
        Ok(h.finalize())
    }
}

impl Default for QuickXorHash {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_is_encoded_zero_length() {
        // 20 zero bytes with length 0 XORed in => all zeros => base64 of 20 zeros.
        assert_eq!(QuickXorHash::hash(b""), "AAAAAAAAAAAAAAAAAAAAAAAAAAA=");
    }

    #[test]
    fn length_affects_digest() {
        // Same content bytes at different lengths must differ (length is mixed in).
        assert_ne!(QuickXorHash::hash(b"\0"), QuickXorHash::hash(b"\0\0"));
    }

    #[test]
    fn incremental_equals_one_shot() {
        let data: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
        let mut h = QuickXorHash::new();
        for chunk in data.chunks(7919) {
            h.update(chunk);
        }
        assert_eq!(h.finalize(), QuickXorHash::hash(&data));
    }

    #[test]
    fn hash_file_matches_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.bin");
        let data: Vec<u8> = (0..=255u8).cycle().take(200_001).collect();
        std::fs::write(&path, &data).unwrap();
        assert_eq!(
            QuickXorHash::hash_file(&path).unwrap(),
            QuickXorHash::hash(&data)
        );
    }

    #[test]
    fn digest_is_28_base64_chars() {
        assert_eq!(QuickXorHash::hash(b"hello world").len(), 28);
    }
}
