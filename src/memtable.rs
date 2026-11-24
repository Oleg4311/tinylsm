//! Memtable: изменяемая часть в памяти. Хранит все версии записей, упорядоченные как в SSTable:
//! по ключу по возрастанию, внутри ключа — новые версии раньше.

use std::cmp::Reverse;
use std::collections::BTreeMap;

use crate::batch::Kind;
use crate::sstable::Entry;

type MemKey = (Vec<u8>, Reverse<u64>);

#[derive(Default)]
pub struct MemTable {
    map: BTreeMap<MemKey, (Kind, Vec<u8>)>,
    bytes: usize,
}

impl MemTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: Vec<u8>, seq: u64, kind: Kind, value: Vec<u8>) {
        self.bytes += key.len() + value.len() + 32;
        self.map.insert((key, Reverse(seq)), (kind, value));
    }

    pub fn approximate_bytes(&self) -> usize {
        self.bytes
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Самая новая версия ключа с номером не больше `max_seq`.
    pub fn get(&self, key: &[u8], max_seq: u64) -> Option<(u64, Kind, &[u8])> {
        let lo = (key.to_vec(), Reverse(max_seq));
        let hi = (key.to_vec(), Reverse(0));
        self.map
            .range(lo..=hi)
            .next()
            .map(|((_, Reverse(seq)), (kind, v))| (*seq, *kind, v.as_slice()))
    }

    /// Все записи по порядку.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], u64, Kind, &[u8])> {
        self.map
            .iter()
            .map(|((k, Reverse(s)), (kind, v))| (k.as_slice(), *s, *kind, v.as_slice()))
    }

    /// Копия записей с ключами из `[start, end)`.
    pub fn entries_in(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Vec<Entry> {
        let from = (
            start.map(<[u8]>::to_vec).unwrap_or_default(),
            Reverse(u64::MAX),
        );
        self.map
            .range(from..)
            .take_while(|((k, _), _)| end.is_none_or(|e| k.as_slice() < e))
            .map(|((k, Reverse(s)), (kind, v))| Entry {
                key: k.clone(),
                seq: *s,
                kind: *kind,
                value: v.clone(),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_snapshots() {
        let mut m = MemTable::new();
        m.insert(b"a".to_vec(), 1, Kind::Put, b"v1".to_vec());
        m.insert(b"a".to_vec(), 5, Kind::Delete, vec![]);
        m.insert(b"a".to_vec(), 9, Kind::Put, b"v9".to_vec());
        m.insert(b"b".to_vec(), 2, Kind::Put, b"b2".to_vec());
        assert_eq!(m.get(b"a", u64::MAX).unwrap().2, b"v9");
        assert_eq!(m.get(b"a", 8).unwrap().1, Kind::Delete);
        assert_eq!(m.get(b"a", 4).unwrap().2, b"v1");
        assert!(m.get(b"a", 0).is_none());
        assert!(m.get(b"c", u64::MAX).is_none());
        assert_eq!(m.get(b"b", 100).unwrap().0, 2);
    }

    #[test]
    fn iteration_order_and_ranges() {
        let mut m = MemTable::new();
        for (k, s) in [("b", 1u64), ("a", 3), ("a", 7), ("c", 2), ("b", 4)] {
            m.insert(k.as_bytes().to_vec(), s, Kind::Put, vec![]);
        }
        let order: Vec<(String, u64)> = m
            .iter()
            .map(|(k, s, _, _)| (String::from_utf8(k.to_vec()).unwrap(), s))
            .collect();
        assert_eq!(
            order,
            vec![
                ("a".into(), 7),
                ("a".into(), 3),
                ("b".into(), 4),
                ("b".into(), 1),
                ("c".into(), 2)
            ]
        );
        let r = m.entries_in(Some(b"b"), Some(b"c"));
        assert_eq!(r.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![4, 1]);
        assert_eq!(m.entries_in(None, None).len(), 5);
        assert!(m.entries_in(Some(b"d"), None).is_empty());
    }
}
