//! Пакет записей (WriteBatch): набор вставок и удалений, применяемый атомарно: после сбоя виден целиком или не виден совсем.

use crate::coding::{Cursor, put_bytes, put_u32, put_u64};
use crate::error::{Error, Result, corruption};

pub const MAX_KEY_LEN: usize = 64 * 1024;
pub const MAX_VALUE_LEN: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Delete = 0,
    Put = 1,
}

impl Kind {
    pub fn from_u8(b: u8) -> Result<Kind> {
        match b {
            0 => Ok(Kind::Delete),
            1 => Ok(Kind::Put),
            _ => corruption(format!("неизвестный тип записи {b}")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Op {
    pub kind: Kind,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteBatch {
    pub(crate) ops: Vec<Op>,
}

impl WriteBatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put(&mut self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> &mut Self {
        self.ops.push(Op {
            kind: Kind::Put,
            key: key.into(),
            value: value.into(),
        });
        self
    }

    pub fn delete(&mut self, key: impl Into<Vec<u8>>) -> &mut Self {
        self.ops.push(Op {
            kind: Kind::Delete,
            key: key.into(),
            value: Vec::new(),
        });
        self
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    pub(crate) fn validate(&self) -> Result<()> {
        for op in &self.ops {
            if op.key.is_empty() || op.key.len() > MAX_KEY_LEN {
                return Err(Error::InvalidArgument(format!(
                    "длина ключа должна быть от 1 до {MAX_KEY_LEN} байт"
                )));
            }
            if op.value.len() > MAX_VALUE_LEN {
                return Err(Error::InvalidArgument(format!(
                    "значение длиннее {MAX_VALUE_LEN} байт"
                )));
            }
        }
        Ok(())
    }

    /// Запись для журнала: первый номер последовательности, число операций, сами операции.
    pub(crate) fn encode(&self, first_seq: u64) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            16 + self
                .ops
                .iter()
                .map(|o| o.key.len() + o.value.len() + 8)
                .sum::<usize>(),
        );
        put_u64(&mut out, first_seq);
        put_u32(&mut out, self.ops.len() as u32);
        for op in &self.ops {
            out.push(op.kind as u8);
            put_bytes(&mut out, &op.key);
            if op.kind == Kind::Put {
                put_bytes(&mut out, &op.value);
            }
        }
        out
    }

    pub(crate) fn decode(payload: &[u8]) -> Result<(u64, WriteBatch)> {
        let mut c = Cursor::new(payload);
        let seq = c.u64()?;
        let count = c.u32()? as usize;
        // число операций не больше числа оставшихся байт: защита от огромного заявленного count
        if count > c.remaining() {
            return corruption("число операций в записи журнала превышает её размер");
        }
        let mut ops = Vec::with_capacity(count);
        for _ in 0..count {
            let kind = Kind::from_u8(c.u8()?)?;
            let key = c.bytes()?.to_vec();
            let value = if kind == Kind::Put {
                c.bytes()?.to_vec()
            } else {
                Vec::new()
            };
            ops.push(Op { kind, key, value });
        }
        if !c.is_empty() {
            return corruption("в записи журнала лишние байты");
        }
        Ok((seq, WriteBatch { ops }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut b = WriteBatch::new();
        b.put("a", "1")
            .delete("b")
            .put(vec![0u8, 255], vec![])
            .put("long", vec![7u8; 100_000]);
        let (seq, back) = WriteBatch::decode(&b.encode(42)).unwrap();
        assert_eq!(seq, 42);
        assert_eq!(back, b);
    }

    #[test]
    fn decode_rejects_garbage_without_panicking() {
        let mut b = WriteBatch::new();
        b.put("key", "value").delete("gone");
        let good = b.encode(1);
        for cut in 0..good.len() {
            assert!(
                WriteBatch::decode(&good[..cut]).is_err(),
                "обрезка до {cut} байт должна отвергаться"
            );
        }
        let mut extra = good.clone();
        extra.push(0);
        assert!(WriteBatch::decode(&extra).is_err());
        let mut huge = good.clone();
        huge[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(WriteBatch::decode(&huge).is_err());
        let mut bad_kind = good;
        bad_kind[12] = 9;
        assert!(WriteBatch::decode(&bad_kind).is_err());
    }

    #[test]
    fn validation_limits() {
        let mut b = WriteBatch::new();
        b.put("", "x");
        assert!(matches!(b.validate(), Err(Error::InvalidArgument(_))));
        let mut b = WriteBatch::new();
        b.put(vec![1u8; MAX_KEY_LEN + 1], "x");
        assert!(b.validate().is_err());
        let mut b = WriteBatch::new();
        b.put("k", vec![0u8; MAX_VALUE_LEN + 1]);
        assert!(b.validate().is_err());
        let mut b = WriteBatch::new();
        b.put("k", vec![0u8; MAX_VALUE_LEN])
            .put(vec![1u8; MAX_KEY_LEN], "")
            .delete("z");
        assert!(b.validate().is_ok());
    }
}
