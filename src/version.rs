//! Версии: набор файлов по уровням и журнал их изменений (манифест).

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::coding::{Cursor, put_bytes, put_u32, put_u64};
use crate::error::{Result, corruption};
use crate::sstable::Table;

pub const NUM_LEVELS: usize = 7;

/// Сведения о файле, которые хранятся в манифесте.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSpec {
    pub number: u64,
    pub size: u64,
    pub entries: u64,
    pub smallest: Vec<u8>,
    pub largest: Vec<u8>,
}

pub struct FileMeta {
    pub spec: FileSpec,
    pub table: Arc<Table>,
}

impl FileMeta {
    pub fn overlaps(&self, lo: &[u8], hi: &[u8]) -> bool {
        self.spec.smallest.as_slice() <= hi && self.spec.largest.as_slice() >= lo
    }
}

/// Изменение версии: добавленные и удалённые файлы плюс сквозные счётчики.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VersionEdit {
    pub log_number: u64,
    pub next_file: u64,
    pub last_seq: u64,
    pub deleted: Vec<(usize, u64)>,
    pub added: Vec<(usize, FileSpec)>,
}

impl VersionEdit {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        put_u64(&mut o, self.log_number);
        put_u64(&mut o, self.next_file);
        put_u64(&mut o, self.last_seq);
        put_u32(&mut o, self.deleted.len() as u32);
        for (l, n) in &self.deleted {
            put_u32(&mut o, *l as u32);
            put_u64(&mut o, *n);
        }
        put_u32(&mut o, self.added.len() as u32);
        for (l, f) in &self.added {
            put_u32(&mut o, *l as u32);
            put_u64(&mut o, f.number);
            put_u64(&mut o, f.size);
            put_u64(&mut o, f.entries);
            put_bytes(&mut o, &f.smallest);
            put_bytes(&mut o, &f.largest);
        }
        o
    }

    pub fn decode(data: &[u8]) -> Result<VersionEdit> {
        let mut c = Cursor::new(data);
        let mut e = VersionEdit {
            log_number: c.u64()?,
            next_file: c.u64()?,
            last_seq: c.u64()?,
            ..Default::default()
        };
        let nd = c.u32()? as usize;
        if nd > c.remaining() {
            return corruption("манифест: слишком много удалений");
        }
        for _ in 0..nd {
            e.deleted.push((c.u32()? as usize, c.u64()?));
        }
        let na = c.u32()? as usize;
        if na > c.remaining() {
            return corruption("манифест: слишком много добавлений");
        }
        for _ in 0..na {
            let level = c.u32()? as usize;
            let f = FileSpec {
                number: c.u64()?,
                size: c.u64()?,
                entries: c.u64()?,
                smallest: c.bytes()?.to_vec(),
                largest: c.bytes()?.to_vec(),
            };
            e.added.push((level, f));
        }
        if !c.is_empty() {
            return corruption("манифест: лишние байты в записи");
        }
        Ok(e)
    }
}

/// Состояние, восстановленное из манифеста.
#[derive(Default)]
pub struct ManifestState {
    pub log_number: u64,
    pub next_file: u64,
    pub last_seq: u64,
    pub files: BTreeMap<u64, (usize, FileSpec)>,
}

impl ManifestState {
    pub fn apply(&mut self, e: &VersionEdit) -> Result<()> {
        self.log_number = self.log_number.max(e.log_number);
        self.next_file = self.next_file.max(e.next_file);
        self.last_seq = self.last_seq.max(e.last_seq);
        for (level, number) in &e.deleted {
            match self.files.get(number) {
                Some((l, _)) if l == level => {
                    self.files.remove(number);
                }
                _ => {
                    return corruption(format!(
                        "манифест удаляет файл {number}, которого нет на уровне {level}"
                    ));
                }
            }
        }
        for (level, f) in &e.added {
            if *level >= NUM_LEVELS {
                return corruption(format!("манифест: уровень {level} вне допустимого"));
            }
            if f.smallest > f.largest {
                return corruption("манифест: границы файла перепутаны");
            }
            if self.files.contains_key(&f.number) {
                return corruption(format!("манифест дважды добавляет файл {}", f.number));
            }
            self.files.insert(f.number, (*level, f.clone()));
        }
        Ok(())
    }

    /// Полный снимок состояния одной правкой (для нового манифеста).
    pub fn to_edit(&self) -> VersionEdit {
        VersionEdit {
            log_number: self.log_number,
            next_file: self.next_file,
            last_seq: self.last_seq,
            deleted: Vec::new(),
            added: self.files.values().map(|(l, f)| (*l, f.clone())).collect(),
        }
    }
}

/// Неизменяемый набор файлов по уровням. L0 — по возрастанию номера (старые первыми), остальные — по ключам.
#[derive(Default)]
pub struct Version {
    pub levels: Vec<Vec<Arc<FileMeta>>>,
}

impl Version {
    pub fn build(files: Vec<(usize, Arc<FileMeta>)>) -> Result<Version> {
        let mut levels: Vec<Vec<Arc<FileMeta>>> = vec![Vec::new(); NUM_LEVELS];
        for (l, f) in files {
            levels[l].push(f);
        }
        levels[0].sort_by_key(|f| f.spec.number);
        for (l, files) in levels.iter_mut().enumerate().skip(1) {
            files.sort_by(|a, b| a.spec.smallest.cmp(&b.spec.smallest));
            for w in files.windows(2) {
                if w[0].spec.largest >= w[1].spec.smallest {
                    return corruption(format!(
                        "файлы {} и {} на уровне {l} пересекаются",
                        w[0].spec.number, w[1].spec.number
                    ));
                }
            }
        }
        Ok(Version { levels })
    }

    pub fn with_edit(
        &self,
        deleted: &[u64],
        added: Vec<(usize, Arc<FileMeta>)>,
    ) -> Result<Version> {
        let mut files: Vec<(usize, Arc<FileMeta>)> = Vec::new();
        for (l, level) in self.levels.iter().enumerate() {
            for f in level {
                if !deleted.contains(&f.spec.number) {
                    files.push((l, f.clone()));
                }
            }
        }
        files.extend(added);
        Version::build(files)
    }

    pub fn level_bytes(&self, level: usize) -> u64 {
        self.levels[level].iter().map(|f| f.spec.size).sum()
    }

    pub fn deepest_nonempty(&self) -> usize {
        (0..NUM_LEVELS)
            .rev()
            .find(|&l| !self.levels[l].is_empty())
            .unwrap_or(0)
    }

    /// Файл уровня ≥ 1, в диапазон которого может попасть ключ.
    pub fn find_file(&self, level: usize, key: &[u8]) -> Option<&Arc<FileMeta>> {
        let files = &self.levels[level];
        let i = files.partition_point(|f| f.spec.largest.as_slice() < key);
        files.get(i).filter(|f| f.spec.smallest.as_slice() <= key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(n: u64, a: &str, b: &str) -> FileSpec {
        FileSpec {
            number: n,
            size: 10,
            entries: 1,
            smallest: a.into(),
            largest: b.into(),
        }
    }

    #[test]
    fn edit_roundtrip_and_garbage() {
        let e = VersionEdit {
            log_number: 4,
            next_file: 9,
            last_seq: 77,
            deleted: vec![(0, 1), (1, 2)],
            added: vec![(1, spec(5, "a", "m")), (2, spec(6, "n", "z"))],
        };
        let enc = e.encode();
        assert_eq!(VersionEdit::decode(&enc).unwrap(), e);
        for cut in 0..enc.len() {
            assert!(VersionEdit::decode(&enc[..cut]).is_err());
        }
        let mut more = enc.clone();
        more.push(0);
        assert!(VersionEdit::decode(&more).is_err());
        let mut huge = enc;
        huge[24..28].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(VersionEdit::decode(&huge).is_err());
    }

    #[test]
    fn manifest_state_rejects_inconsistent_edits() {
        let mut m = ManifestState::default();
        m.apply(&VersionEdit {
            added: vec![(0, spec(1, "a", "b"))],
            next_file: 2,
            ..Default::default()
        })
        .unwrap();
        assert!(
            m.apply(&VersionEdit {
                added: vec![(1, spec(1, "a", "b"))],
                ..Default::default()
            })
            .is_err()
        ); // дубль
        assert!(
            m.apply(&VersionEdit {
                deleted: vec![(1, 1)],
                ..Default::default()
            })
            .is_err()
        ); // не тот уровень
        assert!(
            m.apply(&VersionEdit {
                deleted: vec![(0, 99)],
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            m.apply(&VersionEdit {
                added: vec![(NUM_LEVELS, spec(3, "a", "b"))],
                ..Default::default()
            })
            .is_err()
        );
        assert!(
            m.apply(&VersionEdit {
                added: vec![(1, spec(3, "z", "a"))],
                ..Default::default()
            })
            .is_err()
        );
        m.apply(&VersionEdit {
            deleted: vec![(0, 1)],
            log_number: 3,
            ..Default::default()
        })
        .unwrap();
        assert!(m.files.is_empty());
        assert_eq!((m.log_number, m.next_file), (3, 2));
    }
}
