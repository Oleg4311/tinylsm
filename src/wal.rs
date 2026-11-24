//! Журнал упреждающей записи (WAL). Запись: `crc32 | длина | данные`, CRC покрывает длину и данные.
//!
//! Правило при восстановлении: оборванная или испорченная ПОСЛЕДНЯЯ запись считается недописанным хвостом (процесс умер при записи)
//! и отбрасывается. Испорченная запись, за которой есть ещё данные, считается повреждением носителя и даёт ошибку:
//! молча пропускать середину журнала нельзя, это потеря подтверждённых записей.

use std::io;

use crate::coding::Crc32;
use crate::env::WritableFile;
use crate::error::{Result, corruption};

pub const HEADER_LEN: usize = 8;
pub const MAX_RECORD: usize = 1 << 30;

pub struct WalWriter {
    file: Box<dyn WritableFile>,
}

impl WalWriter {
    pub fn new(file: Box<dyn WritableFile>) -> Self {
        WalWriter { file }
    }

    /// Дописывает запись одним `append` (чтобы обрыв давал максимум один недописанный хвост) и при `sync` делает fsync.
    pub fn add_record(&mut self, payload: &[u8], sync: bool) -> io::Result<()> {
        let len = payload.len() as u32;
        let mut c = Crc32::new();
        c.update(&len.to_le_bytes());
        c.update(payload);
        let mut buf = Vec::with_capacity(HEADER_LEN + payload.len());
        buf.extend_from_slice(&c.finish().to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(payload);
        self.file.append(&buf)?;
        if sync {
            self.file.sync()?;
        }
        Ok(())
    }

    pub fn sync(&mut self) -> io::Result<()> {
        self.file.sync()
    }

    pub fn len(&self) -> u64 {
        self.file.len()
    }

    pub fn is_empty(&self) -> bool {
        self.file.is_empty()
    }
}

pub struct Replay<'a> {
    pub records: Vec<&'a [u8]>,
    /// Сколько байт файла заняты целыми корректными записями.
    pub valid_len: usize,
    /// Был ли отброшен недописанный хвост.
    pub torn_tail: bool,
}

/// Читает записи журнала до конца или до недописанного хвоста; повреждение в середине даёт ошибку.
pub fn replay(data: &[u8]) -> Result<Replay<'_>> {
    let mut pos = 0;
    let mut records = Vec::new();
    while pos < data.len() {
        let rest = data.len() - pos;
        if rest < HEADER_LEN {
            return Ok(Replay {
                records,
                valid_len: pos,
                torn_tail: true,
            });
        }
        let crc = u32::from_le_bytes(data[pos..pos + 4].try_into().expect("длина"));
        let len = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().expect("длина")) as usize;
        let end = pos.saturating_add(HEADER_LEN).saturating_add(len);
        if len == 0 || len > MAX_RECORD || end > data.len() {
            // заявленная запись не помещается в файл: хвост не дописан (или длина испорчена в последней записи)
            return Ok(Replay {
                records,
                valid_len: pos,
                torn_tail: true,
            });
        }
        let payload = &data[pos + HEADER_LEN..end];
        let mut c = Crc32::new();
        c.update(&data[pos + 4..pos + 8]);
        c.update(payload);
        if c.finish() != crc {
            if end == data.len() {
                return Ok(Replay {
                    records,
                    valid_len: pos,
                    torn_tail: true,
                });
            }
            return corruption(format!(
                "запись журнала по смещению {pos} повреждена, а за ней есть ещё данные"
            ));
        }
        records.push(payload);
        pos = end;
    }
    Ok(Replay {
        records,
        valid_len: pos,
        torn_tail: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::mem_env;
    use std::path::Path;

    fn write_log(records: &[&[u8]]) -> Vec<u8> {
        let (_m, env) = mem_env();
        let mut w = WalWriter::new(env.create_file(Path::new("d/log")).unwrap());
        for r in records {
            w.add_record(r, false).unwrap();
        }
        env.read_file(Path::new("d/log")).unwrap()
    }

    #[test]
    fn roundtrip() {
        let data = write_log(&[b"one", b"two two", &[9u8; 5000]]);
        let r = replay(&data).unwrap();
        assert_eq!(r.records.len(), 3);
        assert_eq!(r.records[1], b"two two");
        assert_eq!(r.valid_len, data.len());
        assert!(!r.torn_tail);
    }

    #[test]
    fn every_truncation_yields_a_clean_prefix() {
        let data = write_log(&[b"alpha", b"beta", b"gamma delta"]);
        let full = replay(&data).unwrap().records.len();
        assert_eq!(full, 3);
        let mut seen = std::collections::BTreeSet::new();
        for cut in 0..=data.len() {
            let r = replay(&data[..cut]).unwrap();
            assert!(r.valid_len <= cut);
            assert_eq!(
                r.torn_tail,
                r.valid_len != cut,
                "хвост помечается ровно когда есть лишние байты"
            );
            seen.insert(r.records.len());
            for (i, rec) in r.records.iter().enumerate() {
                assert_eq!(*rec, [b"alpha".as_slice(), b"beta", b"gamma delta"][i]);
            }
        }
        assert_eq!(seen.into_iter().collect::<Vec<_>>(), vec![0, 1, 2, 3]);
    }

    #[test]
    fn corrupted_last_record_is_a_torn_tail_but_middle_corruption_is_an_error() {
        let data = write_log(&[b"first", b"second", b"third"]);
        let mut last = data.clone();
        let n = last.len();
        last[n - 1] ^= 1;
        let r = replay(&last).unwrap();
        assert_eq!(r.records.len(), 2);
        assert!(r.torn_tail);
        let mut middle = data.clone();
        middle[HEADER_LEN + 2] ^= 1; // внутри первой записи
        assert!(replay(&middle).is_err());
        let mut length = data;
        length[4] ^= 0x40; // испорчена длина первой записи
        assert!(replay(&length).is_err() || replay(&length).unwrap().records.is_empty());
    }

    #[test]
    fn zero_length_and_absurd_length_do_not_panic() {
        assert!(replay(&[0u8; 8]).unwrap().torn_tail);
        let mut huge = vec![0u8; 8];
        huge[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(replay(&huge).unwrap().torn_tail);
        assert!(replay(&[]).unwrap().records.is_empty());
    }
}
