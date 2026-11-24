//! CRC-32 (IEEE 802.3, как в zlib и PNG) и кодирование целых: varint и фиксированной длины.

use crate::error::{Error, Result, corruption};

const POLY: u32 = 0xEDB8_8320;

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static TABLE: [u32; 256] = make_table();

/// CRC-32 от среза.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finish()
}

/// Накопительный CRC-32: данные можно подавать частями.
#[derive(Clone, Copy)]
pub struct Crc32(u32);

impl Crc32 {
    pub fn new() -> Self {
        Crc32(0xFFFF_FFFF)
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c = TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    pub fn finish(&self) -> u32 {
        self.0 ^ 0xFFFF_FFFF
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// Курсор по байтам с проверкой границ: любая нехватка данных даёт ошибку повреждения, а не панику.
pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn varint(&mut self) -> Result<u64> {
        let mut result = 0u64;
        let mut shift = 0u32;
        loop {
            let Some(&b) = self.data.get(self.pos) else {
                return corruption("varint оборван");
            };
            self.pos += 1;
            if shift >= 64 || (shift == 63 && b > 1) {
                return corruption("varint длиннее 64 бит");
            }
            result |= u64::from(b & 0x7F) << shift;
            if b & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }

    pub fn u8(&mut self) -> Result<u8> {
        let b = self.take(1)?;
        Ok(b[0])
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes(b.try_into().expect("длина проверена")))
    }

    pub fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes(b.try_into().expect("длина проверена")))
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return corruption(format!("ожидалось {n} байт, осталось {}", self.remaining()));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    /// Строка с префиксом длины (varint). Длина проверяется до выделения памяти.
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.varint()?;
        let n = usize::try_from(n)
            .map_err(|_| Error::Corruption("длина не помещается в usize".into()))?;
        self.take(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_reference_vectors() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(
            crc32(b"The quick brown fox jumps over the lazy dog"),
            0x414F_A339
        );
        assert_eq!(crc32(&[0u8; 32]), 0x190A_55AD);
    }

    #[test]
    fn crc32_streaming_equals_one_shot() {
        let data: Vec<u8> = (0..1000u32).map(|i| (i * 7 + 3) as u8).collect();
        let mut c = Crc32::new();
        for chunk in data.chunks(13) {
            c.update(chunk);
        }
        assert_eq!(c.finish(), crc32(&data));
    }

    #[test]
    fn varint_roundtrip_and_boundaries() {
        for v in [
            0u64,
            1,
            127,
            128,
            16383,
            16384,
            u32::MAX as u64,
            u64::MAX - 1,
            u64::MAX,
        ] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            let mut c = Cursor::new(&buf);
            assert_eq!(c.varint().unwrap(), v);
            assert!(c.is_empty());
        }
    }

    #[test]
    fn cursor_rejects_malformed_input_without_panicking() {
        assert!(Cursor::new(&[0x80]).varint().is_err()); // оборван
        assert!(Cursor::new(&[0xFF; 11]).varint().is_err()); // слишком длинный
        assert!(Cursor::new(&[1, 2, 3]).u32().is_err());
        assert!(Cursor::new(&[5, b'a']).bytes().is_err()); // длина больше остатка
        let mut huge = Vec::new();
        put_varint(&mut huge, u64::MAX);
        assert!(Cursor::new(&huge).bytes().is_err()); // огромная длина не приводит к выделению памяти
    }
}
