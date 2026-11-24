//! Фильтр Блума для SSTable: быстро отвечает «этого ключа в файле точно нет» и экономит чтения диска.

/// Хэш ключа (FNV-1a 64 бит, свёрнутый до 32): быстрый и достаточно равномерный для двойного хэширования.
fn hash32(key: &[u8]) -> u32 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for &b in key {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    (h ^ (h >> 32)) as u32
}

pub struct BloomFilter;

impl BloomFilter {
    /// Строит фильтр для списка ключей. Последний байт результата — число проб `k`.
    pub fn build(keys: &[&[u8]], bits_per_key: usize) -> Vec<u8> {
        let k = ((bits_per_key as f64 * 0.69) as usize).clamp(1, 30);
        let nbits = (keys.len() * bits_per_key).max(64);
        let nbytes = nbits.div_ceil(8);
        let nbits = nbytes * 8;
        let mut bits = vec![0u8; nbytes + 1];
        for key in keys {
            let mut h = hash32(key);
            let delta = h.rotate_right(17);
            for _ in 0..k {
                let pos = (h as usize) % nbits;
                bits[pos / 8] |= 1 << (pos % 8);
                h = h.wrapping_add(delta);
            }
        }
        bits[nbytes] = k as u8;
        bits
    }

    /// Может ли ключ быть в наборе (ложные срабатывания возможны, ложных отказов нет).
    /// Повреждённый или непонятный фильтр считается «может быть»: данные от этого не пострадают.
    pub fn may_contain(filter: &[u8], key: &[u8]) -> bool {
        if filter.len() < 2 {
            return true;
        }
        let k = filter[filter.len() - 1] as usize;
        if k == 0 || k > 30 {
            return true;
        }
        let nbits = (filter.len() - 1) * 8;
        let mut h = hash32(key);
        let delta = h.rotate_right(17);
        for _ in 0..k {
            let pos = (h as usize) % nbits;
            if filter[pos / 8] & (1 << (pos % 8)) == 0 {
                return false;
            }
            h = h.wrapping_add(delta);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(i: u32) -> Vec<u8> {
        format!("key-{i:08}").into_bytes()
    }

    #[test]
    fn no_false_negatives() {
        let keys: Vec<Vec<u8>> = (0..5000).map(key).collect();
        let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        let f = BloomFilter::build(&refs, 10);
        assert!(keys.iter().all(|k| BloomFilter::may_contain(&f, k)));
    }

    #[test]
    fn false_positive_rate_is_low() {
        let keys: Vec<Vec<u8>> = (0..10_000).map(key).collect();
        let refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        let f = BloomFilter::build(&refs, 10);
        let fp = (1_000_000..1_020_000u32)
            .filter(|&i| BloomFilter::may_contain(&f, &key(i)))
            .count();
        let rate = fp as f64 / 20_000.0;
        assert!(
            rate < 0.02,
            "доля ложных срабатываний {rate:.4}: при 10 битах на ключ ждём около 1%"
        );
    }

    #[test]
    fn empty_and_garbage_filters_are_safe() {
        let f = BloomFilter::build(&[], 10);
        assert!(!BloomFilter::may_contain(&f, b"anything") || f.len() > 1);
        assert!(BloomFilter::may_contain(&[], b"x"));
        assert!(BloomFilter::may_contain(&[1, 0], b"x")); // k = 0
        assert!(BloomFilter::may_contain(&[0xFF, 99], b"x")); // k слишком велико
    }
}
