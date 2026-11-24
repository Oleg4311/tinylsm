//! SSTable: неизменяемый отсортированный файл.
//!
//! Раскладка: `[блоки данных][блок индекса][блок фильтра Блума][подвал]`. У каждого блока в конце CRC-32;
//! подвал фиксированной длины с магическим числом и собственной суммой. Записи упорядочены по (ключ, номер ↓):
//! для одного ключа сначала самая новая версия.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::batch::Kind;
use crate::bloom::BloomFilter;
use crate::coding::{Cursor, crc32, put_bytes, put_u32, put_u64};
use crate::env::{Env, RandomAccessFile, WritableFile};
use crate::error::{Error, Result, corruption};

const MAGIC: u64 = 0x544C_534D_5441_4231; // «TLSMTAB1»
const FOOTER_LEN: usize = 8 + 4 + 8 + 4 + 8 + 8 + 4;
pub const BLOCK_SIZE: usize = 4096;
const BLOOM_BITS_PER_KEY: usize = 10;
const BLOCK_CACHE_CAP: usize = 128;

/// Запись во внутреннем виде: пользовательский ключ, номер версии, вид, значение.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub key: Vec<u8>,
    pub seq: u64,
    pub kind: Kind,
    pub value: Vec<u8>,
}

/// Порядок записей: по ключу по возрастанию, при равных ключах новые версии (больший номер) раньше.
pub fn cmp_internal(a_key: &[u8], a_seq: u64, b_key: &[u8], b_seq: u64) -> Ordering {
    a_key.cmp(b_key).then(b_seq.cmp(&a_seq))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableInfo {
    pub size: u64,
    pub entries: u64,
    pub smallest: Vec<u8>,
    pub largest: Vec<u8>,
}

struct IndexEntry {
    last_key: Vec<u8>,
    last_seq: u64,
    offset: u64,
    size: u32,
}

pub struct TableBuilder {
    file: Box<dyn WritableFile>,
    block: Vec<u8>,
    block_last: Option<(Vec<u8>, u64)>,
    index: Vec<IndexEntry>,
    bloom_keys: Vec<Vec<u8>>,
    offset: u64,
    entries: u64,
    last: Option<(Vec<u8>, u64)>,
    smallest: Option<Vec<u8>>,
}

impl TableBuilder {
    /// Сколько байт займёт файл, если закончить прямо сейчас (без индекса и подвала).
    pub fn size_estimate(&self) -> u64 {
        self.offset + self.block.len() as u64
    }

    pub fn new(file: Box<dyn WritableFile>) -> Self {
        TableBuilder {
            file,
            block: Vec::new(),
            block_last: None,
            index: Vec::new(),
            bloom_keys: Vec::new(),
            offset: 0,
            entries: 0,
            last: None,
            smallest: None,
        }
    }

    /// Добавляет запись. Порядок возрастания (ключ, номер ↓) обязателен: нарушение — ошибка программиста, а не данных.
    pub fn add(&mut self, key: &[u8], seq: u64, kind: Kind, value: &[u8]) -> Result<()> {
        if let Some((lk, ls)) = &self.last
            && cmp_internal(lk, *ls, key, seq) != Ordering::Less
        {
            return Err(Error::InvalidArgument(
                "записи SSTable должны идти строго по порядку".into(),
            ));
        }
        if self.last.as_ref().is_none_or(|(lk, _)| lk != key) {
            self.bloom_keys.push(key.to_vec());
        }
        self.smallest.get_or_insert_with(|| key.to_vec());
        put_bytes(&mut self.block, key);
        put_u64(&mut self.block, seq);
        self.block.push(kind as u8);
        put_bytes(&mut self.block, value);
        self.block_last = Some((key.to_vec(), seq));
        self.last = self.block_last.clone();
        self.entries += 1;
        if self.block.len() >= BLOCK_SIZE {
            self.flush_block()?;
        }
        Ok(())
    }

    fn write_checked(&mut self, mut data: Vec<u8>) -> Result<(u64, u32)> {
        let crc = crc32(&data);
        put_u32(&mut data, crc);
        let off = self.offset;
        self.file.append(&data)?;
        self.offset += data.len() as u64;
        Ok((off, data.len() as u32))
    }

    fn flush_block(&mut self) -> Result<()> {
        if self.block.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.block);
        let (last_key, last_seq) = self.block_last.take().expect("в блоке есть записи");
        let (offset, size) = self.write_checked(data)?;
        self.index.push(IndexEntry {
            last_key,
            last_seq,
            offset,
            size,
        });
        Ok(())
    }

    /// Дописывает индекс, фильтр и подвал; при `sync` делает fsync файла.
    pub fn finish(mut self, sync: bool) -> Result<TableInfo> {
        self.flush_block()?;
        let mut idx = Vec::new();
        for e in &self.index {
            put_bytes(&mut idx, &e.last_key);
            put_u64(&mut idx, e.last_seq);
            put_u64(&mut idx, e.offset);
            put_u32(&mut idx, e.size);
        }
        let (index_off, index_len) = self.write_checked(idx)?;
        let refs: Vec<&[u8]> = self.bloom_keys.iter().map(|k| k.as_slice()).collect();
        let bloom = BloomFilter::build(&refs, BLOOM_BITS_PER_KEY);
        let (bloom_off, bloom_len) = self.write_checked(bloom)?;
        let mut footer = Vec::with_capacity(FOOTER_LEN);
        put_u64(&mut footer, index_off);
        put_u32(&mut footer, index_len);
        put_u64(&mut footer, bloom_off);
        put_u32(&mut footer, bloom_len);
        put_u64(&mut footer, self.entries);
        put_u64(&mut footer, MAGIC);
        let crc = crc32(&footer);
        put_u32(&mut footer, crc);
        self.file.append(&footer)?;
        self.offset += footer.len() as u64;
        if sync {
            self.file.sync()?;
        }
        let (largest, _) = self.last.clone().unwrap_or_default();
        Ok(TableInfo {
            size: self.offset,
            entries: self.entries,
            smallest: self.smallest.unwrap_or_default(),
            largest,
        })
    }
}

fn parse_entries(block: &[u8]) -> Result<Vec<Entry>> {
    let mut c = Cursor::new(block);
    let mut out = Vec::new();
    while !c.is_empty() {
        let key = c.bytes()?.to_vec();
        let seq = c.u64()?;
        let kind = Kind::from_u8(c.u8()?)?;
        let value = c.bytes()?.to_vec();
        out.push(Entry {
            key,
            seq,
            kind,
            value,
        });
    }
    Ok(out)
}

pub struct Table {
    file: Option<Arc<dyn RandomAccessFile>>,
    /// Если файл признан ненужным, он удаляется, когда исчезнет последний читатель таблицы.
    cleanup: Mutex<Option<(Arc<dyn Env>, PathBuf)>>,
    index: Vec<IndexEntry>,
    bloom: Vec<u8>,
    pub entries: u64,
    cache: Mutex<HashMap<u64, Arc<Vec<u8>>>>,
}

impl Table {
    pub fn open(env: &dyn Env, path: &Path) -> Result<Table> {
        let file = env.open_random(path)?;
        Table::from_file(file)
    }

    pub fn from_file(file: Arc<dyn RandomAccessFile>) -> Result<Table> {
        let size = file.len();
        if size < FOOTER_LEN as u64 {
            return corruption("файл SSTable короче подвала");
        }
        let footer = file.read_at(size - FOOTER_LEN as u64, FOOTER_LEN)?;
        if footer.len() != FOOTER_LEN {
            return corruption("подвал SSTable прочитан не полностью");
        }
        let body = &footer[..FOOTER_LEN - 4];
        if crc32(body) != u32::from_le_bytes(footer[FOOTER_LEN - 4..].try_into().expect("длина"))
        {
            return corruption("контрольная сумма подвала SSTable не сходится");
        }
        let mut c = Cursor::new(body);
        let index_off = c.u64()?;
        let index_len = c.u32()?;
        let bloom_off = c.u64()?;
        let bloom_len = c.u32()?;
        let entries = c.u64()?;
        if c.u64()? != MAGIC {
            return corruption("неверное магическое число SSTable");
        }
        let limit = size - FOOTER_LEN as u64;
        if index_off.saturating_add(u64::from(index_len)) > limit
            || bloom_off.saturating_add(u64::from(bloom_len)) > limit
        {
            return corruption("границы индекса или фильтра выходят за файл");
        }
        let idx_raw = read_checked(&*file, index_off, index_len)?;
        let mut c = Cursor::new(&idx_raw);
        let mut index = Vec::new();
        while !c.is_empty() {
            let last_key = c.bytes()?.to_vec();
            let last_seq = c.u64()?;
            let offset = c.u64()?;
            let size = c.u32()?;
            if offset.saturating_add(u64::from(size)) > index_off {
                return corruption("блок данных выходит за область данных");
            }
            index.push(IndexEntry {
                last_key,
                last_seq,
                offset,
                size,
            });
        }
        let bloom_raw = read_checked(&*file, bloom_off, bloom_len)?;
        Ok(Table {
            file: Some(file),
            cleanup: Mutex::new(None),
            index,
            bloom: bloom_raw,
            entries,
            cache: Mutex::new(HashMap::new()),
        })
    }

    fn block(&self, i: usize) -> Result<Arc<Vec<u8>>> {
        let e = &self.index[i];
        if let Some(b) = self
            .cache
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&e.offset)
        {
            return Ok(b.clone());
        }
        let data = Arc::new(read_checked(
            &**self.file.as_ref().expect("файл открыт"),
            e.offset,
            e.size,
        )?);
        let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
        if cache.len() >= BLOCK_CACHE_CAP {
            cache.clear();
        }
        cache.insert(e.offset, data.clone());
        Ok(data)
    }

    /// Помечает файл ненужным: он будет удалён, когда таблицу перестанут читать.
    pub fn mark_obsolete(&self, env: Arc<dyn Env>, path: PathBuf) {
        *self.cleanup.lock().unwrap_or_else(|p| p.into_inner()) = Some((env, path));
    }

    pub fn may_contain(&self, key: &[u8]) -> bool {
        BloomFilter::may_contain(&self.bloom, key)
    }

    /// Индекс первого блока, в котором может быть запись не раньше (key, seq) во внутреннем порядке.
    fn first_block_at_or_after(&self, key: &[u8], seq: u64) -> usize {
        self.index
            .partition_point(|e| cmp_internal(&e.last_key, e.last_seq, key, seq) == Ordering::Less)
    }

    /// Самая новая версия ключа с номером не больше `max_seq`.
    pub fn get(&self, key: &[u8], max_seq: u64) -> Result<Option<Entry>> {
        if !self.may_contain(key) {
            return Ok(None);
        }
        let mut it = TableIter::new(self);
        it.seek(key, max_seq)?;
        match it.peek() {
            Some(e) if e.key == key => Ok(Some(e.clone())),
            _ => Ok(None),
        }
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        // сначала закрываем файл (на Windows открытый файл не удалить), потом удаляем
        self.file = None;
        if let Some((env, path)) = self
            .cleanup
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            let _ = env.remove(&path);
        }
    }
}

fn read_checked(file: &dyn RandomAccessFile, offset: u64, size: u32) -> Result<Vec<u8>> {
    if size < 4 {
        return corruption("блок короче контрольной суммы");
    }
    let raw = file.read_at(offset, size as usize)?;
    if raw.len() != size as usize {
        return corruption("блок прочитан не полностью");
    }
    let (data, crc) = raw.split_at(raw.len() - 4);
    if crc32(data) != u32::from_le_bytes(crc.try_into().expect("длина")) {
        return corruption(format!(
            "контрольная сумма блока по смещению {offset} не сходится"
        ));
    }
    Ok(data.to_vec())
}

/// Последовательный просмотр таблицы с поиском по (ключ, номер).
pub struct TableIter<T: Deref<Target = Table>> {
    table: T,
    block_idx: usize,
    entries: Vec<Entry>,
    pos: usize,
}

impl<T: Deref<Target = Table>> TableIter<T> {
    pub fn new(table: T) -> Self {
        TableIter {
            table,
            block_idx: 0,
            entries: Vec::new(),
            pos: 0,
        }
    }

    pub fn seek_to_first(&mut self) -> Result<()> {
        self.load(0)
    }

    /// Встаёт на первую запись, не предшествующую (key, seq) во внутреннем порядке.
    pub fn seek(&mut self, key: &[u8], seq: u64) -> Result<()> {
        let b = self.table.first_block_at_or_after(key, seq);
        self.load(b)?;
        while let Some(e) = self.entries.get(self.pos) {
            if cmp_internal(&e.key, e.seq, key, seq) != Ordering::Less {
                break;
            }
            self.pos += 1;
        }
        self.settle()
    }

    fn load(&mut self, idx: usize) -> Result<()> {
        self.block_idx = idx;
        self.pos = 0;
        self.entries = if idx < self.table.index.len() {
            parse_entries(&self.table.block(idx)?)?
        } else {
            Vec::new()
        };
        Ok(())
    }

    /// Если блок исчерпан, переходит к следующему непустому.
    fn settle(&mut self) -> Result<()> {
        while self.pos >= self.entries.len() && self.block_idx < self.table.index.len() {
            self.load(self.block_idx + 1)?;
        }
        Ok(())
    }

    pub fn peek(&self) -> Option<&Entry> {
        self.entries.get(self.pos)
    }

    pub fn advance(&mut self) -> Result<()> {
        self.pos += 1;
        self.settle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::mem_env;

    fn build(entries: &[(&str, u64, Kind, &str)]) -> (Arc<dyn Env>, TableInfo) {
        let (_m, env) = mem_env();
        let mut b = TableBuilder::new(env.create_file(Path::new("d/t")).unwrap());
        for (k, s, kind, v) in entries {
            b.add(k.as_bytes(), *s, *kind, v.as_bytes()).unwrap();
        }
        let info = b.finish(true).unwrap();
        (env, info)
    }

    fn many(n: usize) -> Vec<(String, u64, Kind, String)> {
        (0..n)
            .map(|i| {
                (
                    format!("key-{i:06}"),
                    (i + 1) as u64,
                    Kind::Put,
                    format!("value-{i}-{}", "x".repeat(i % 50)),
                )
            })
            .collect()
    }

    fn build_many(n: usize) -> (Arc<dyn Env>, TableInfo) {
        let data = many(n);
        let refs: Vec<(&str, u64, Kind, &str)> = data
            .iter()
            .map(|(k, s, kd, v)| (k.as_str(), *s, *kd, v.as_str()))
            .collect();
        build(&refs)
    }

    #[test]
    fn point_lookups_and_scan() {
        let (env, info) = build_many(3000);
        assert_eq!(info.entries, 3000);
        assert_eq!(
            (info.smallest.as_slice(), info.largest.as_slice()),
            (b"key-000000".as_slice(), b"key-002999".as_slice())
        );
        let t = Table::open(&*env, Path::new("d/t")).unwrap();
        for i in (0..3000).step_by(7) {
            let e = t
                .get(format!("key-{i:06}").as_bytes(), u64::MAX)
                .unwrap()
                .expect("ключ есть");
            assert_eq!(
                e.value,
                format!("value-{i}-{}", "x".repeat(i % 50)).into_bytes()
            );
        }
        assert!(t.get(b"key-999999", u64::MAX).unwrap().is_none());
        assert!(t.get(b"aaa", u64::MAX).unwrap().is_none());
        let mut it = TableIter::new(&t);
        it.seek_to_first().unwrap();
        let mut n = 0;
        while let Some(e) = it.peek() {
            assert_eq!(e.key, format!("key-{n:06}").into_bytes());
            n += 1;
            it.advance().unwrap();
        }
        assert_eq!(n, 3000);
    }

    #[test]
    fn versions_are_ordered_newest_first_and_snapshots_see_the_right_one() {
        let (env, _) = build(&[
            ("a", 9, Kind::Put, "new"),
            ("a", 5, Kind::Delete, ""),
            ("a", 2, Kind::Put, "old"),
            ("b", 4, Kind::Put, "bee"),
        ]);
        let t = Table::open(&*env, Path::new("d/t")).unwrap();
        assert_eq!(t.get(b"a", u64::MAX).unwrap().unwrap().value, b"new");
        assert_eq!(t.get(b"a", 9).unwrap().unwrap().seq, 9);
        assert_eq!(t.get(b"a", 8).unwrap().unwrap().kind, Kind::Delete);
        assert_eq!(t.get(b"a", 4).unwrap().unwrap().value, b"old");
        assert!(t.get(b"a", 1).unwrap().is_none());
        assert_eq!(t.get(b"b", 100).unwrap().unwrap().value, b"bee");
    }

    #[test]
    fn one_key_spanning_many_blocks_is_found_by_snapshot() {
        let big = "v".repeat(1500);
        let mut data = Vec::new();
        for s in (1..=40u64).rev() {
            data.push(("same", s, Kind::Put, big.as_str()));
        }
        let (env, _) = build(&data);
        let t = Table::open(&*env, Path::new("d/t")).unwrap();
        for s in [1u64, 13, 27, 40] {
            assert_eq!(t.get(b"same", s).unwrap().unwrap().seq, s);
        }
        assert!(t.get(b"same", 0).unwrap().is_none());
    }

    #[test]
    fn builder_rejects_unordered_input() {
        let (_m, env) = mem_env();
        let mut b = TableBuilder::new(env.create_file(Path::new("d/t")).unwrap());
        b.add(b"b", 1, Kind::Put, b"x").unwrap();
        assert!(b.add(b"a", 2, Kind::Put, b"x").is_err());
        assert!(b.add(b"b", 1, Kind::Put, b"x").is_err()); // тот же ключ и номер
        assert!(b.add(b"b", 2, Kind::Put, b"x").is_err()); // номер должен убывать
        b.add(b"b", 0, Kind::Put, b"x").unwrap();
    }

    #[test]
    fn empty_table_is_valid() {
        let (env, info) = build(&[]);
        assert_eq!(info.entries, 0);
        let t = Table::open(&*env, Path::new("d/t")).unwrap();
        assert!(t.get(b"x", u64::MAX).unwrap().is_none());
        let mut it = TableIter::new(&t);
        it.seek_to_first().unwrap();
        assert!(it.peek().is_none());
    }

    #[test]
    fn any_single_byte_corruption_is_detected_when_the_table_is_read() {
        let data = many(400);
        let refs: Vec<(&str, u64, Kind, &str)> = data
            .iter()
            .map(|(k, s, kd, v)| (k.as_str(), *s, *kd, v.as_str()))
            .collect();
        let (m, env) = mem_env();
        let mut b = TableBuilder::new(env.create_file(Path::new("d/t")).unwrap());
        for (k, s, kd, v) in &refs {
            b.add(k.as_bytes(), *s, *kd, v.as_bytes()).unwrap();
        }
        b.finish(true).unwrap();
        let len = m.contents(Path::new("d/t")).unwrap().len();
        let mut silent = 0;
        for off in (0..len).step_by(5) {
            m.corrupt_byte(Path::new("d/t"), off, 0x01);
            let detected = match Table::open(&*env, Path::new("d/t")) {
                Err(_) => true,
                Ok(t) => {
                    let mut it = TableIter::new(&t);
                    it.seek_to_first().is_err() || {
                        let mut bad = false;
                        loop {
                            match it.peek() {
                                None => break,
                                Some(_) => {
                                    if it.advance().is_err() {
                                        bad = true;
                                        break;
                                    }
                                }
                            }
                        }
                        bad
                    }
                }
            };
            m.corrupt_byte(Path::new("d/t"), off, 0x01); // вернули как было
            if !detected {
                silent += 1;
            }
        }
        // фильтр Блума повреждённым считается «может быть»: порча там безвредна, всё остальное обязано обнаруживаться
        assert!(silent <= 2, "незамеченных порч: {silent}");
    }

    #[test]
    fn truncated_files_are_rejected() {
        let (env, _) = build_many(200);
        let m_len = env.read_file(Path::new("d/t")).unwrap().len();
        assert!(m_len > FOOTER_LEN);
        // обрезка через новую копию
        let (m2, env2) = mem_env();
        let data = env.read_file(Path::new("d/t")).unwrap();
        for cut in [0usize, 10, FOOTER_LEN - 1, FOOTER_LEN, m_len / 2, m_len - 1] {
            let mut f = env2.create_file(Path::new("d/c")).unwrap();
            f.append(&data[..cut]).unwrap();
            assert!(
                Table::open(&*env2, Path::new("d/c")).is_err(),
                "обрезка до {cut}"
            );
        }
        let _ = m2;
    }
}
