//! Модельные тесты: случайные последовательности операций сравниваются с обычным `BTreeMap`.
//! Включают сбросы на диск, уплотнение, переоткрытие и снимки; параметры минимальные, чтобы уровни переполнялись постоянно.

use std::collections::BTreeMap;
use std::path::Path;

use proptest::prelude::*;
use tinylsm::env::mem_env;
use tinylsm::{Db, Options, WriteBatch};

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

fn tiny() -> Options {
    Options {
        write_buffer_size: 400,
        l0_compaction_trigger: 2,
        level_base_bytes: 1200,
        target_file_size: 400,
        sync: false,
    }
}

#[derive(Clone, Debug)]
enum Op {
    Put(u8, u8, u8),
    Del(u8),
    Batch(Vec<(u8, Option<u8>)>),
    Get(u8),
    Scan(u8, u8),
    Flush,
    Compact,
    Reopen,
    TakeSnapshot,
    CheckSnapshot(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0u8..20, any::<u8>(), 0u8..80).prop_map(|(k, v, n)| Op::Put(k, v, n)),
        2 => (0u8..20).prop_map(Op::Del),
        2 => prop::collection::vec((0u8..20, prop::option::of(any::<u8>())), 1..5).prop_map(Op::Batch),
        3 => (0u8..22).prop_map(Op::Get),
        2 => (0u8..22, 0u8..22).prop_map(|(a, b)| Op::Scan(a, b)),
        1 => Just(Op::Flush),
        1 => Just(Op::Compact),
        1 => Just(Op::Reopen),
        1 => Just(Op::TakeSnapshot),
        2 => (0usize..4).prop_map(Op::CheckSnapshot),
    ]
}

fn k(i: u8) -> Vec<u8> {
    format!("key{i:02}").into_bytes()
}
fn v(b: u8, n: u8) -> Vec<u8> {
    let mut x = vec![b; 1 + n as usize];
    x.push(n);
    x
}

fn model_scan(m: &Model, a: Option<&[u8]>, b: Option<&[u8]>) -> Vec<(Vec<u8>, Vec<u8>)> {
    m.iter()
        .filter(|(key, _)| {
            a.is_none_or(|a| key.as_slice() >= a) && b.is_none_or(|b| key.as_slice() < b)
        })
        .map(|(a, b)| (a.clone(), b.clone()))
        .collect()
}

fn same(got: &[(Vec<u8>, Vec<u8>)], want: &[(Vec<u8>, Vec<u8>)], what: &str) {
    if got == want {
        return;
    }
    let g: BTreeMap<_, _> = got.iter().cloned().collect();
    let w: BTreeMap<_, _> = want.iter().cloned().collect();
    let mut msg = format!("{what}: расхождения:");
    for key in g
        .keys()
        .chain(w.keys())
        .collect::<std::collections::BTreeSet<_>>()
    {
        if g.get(key) != w.get(key) {
            msg += &format!(
                " [{} база={:?} модель={:?}]",
                String::from_utf8_lossy(key),
                g.get(key).map(|v| v.len()),
                w.get(key).map(|v| v.len())
            );
        }
    }
    panic!("{msg}");
}

fn run_ops(ops: &[Op]) {
    let (_m, env) = mem_env();
    let dir = Path::new("db");
    let mut db = Db::open_with_env(env.clone(), dir, tiny()).unwrap();
    let mut model = Model::new();
    let mut snaps: Vec<(tinylsm::Snapshot, Model)> = Vec::new();
    for op in ops {
        match op {
            Op::Put(a, b, n) => {
                db.put(k(*a), v(*b, *n)).unwrap();
                model.insert(k(*a), v(*b, *n));
            }
            Op::Del(a) => {
                db.delete(k(*a)).unwrap();
                model.remove(&k(*a));
            }
            Op::Batch(items) => {
                let mut batch = WriteBatch::new();
                for (a, val) in items {
                    match val {
                        Some(b) => {
                            batch.put(k(*a), v(*b, 3));
                            model.insert(k(*a), v(*b, 3));
                        }
                        None => {
                            batch.delete(k(*a));
                            model.remove(&k(*a));
                        }
                    }
                }
                db.write(&batch).unwrap();
            }
            Op::Get(a) => assert_eq!(
                db.get(&k(*a)).unwrap(),
                model.get(&k(*a)).cloned(),
                "get {a}"
            ),
            Op::Scan(a, b) => {
                let (lo, hi) = (k(*a), k(*b));
                let got: Vec<_> = db
                    .scan(Some(&lo), Some(&hi))
                    .unwrap()
                    .map(|r| r.unwrap())
                    .collect();
                assert_eq!(
                    got,
                    model_scan(&model, Some(&lo), Some(&hi)),
                    "scan {a}..{b}"
                );
            }
            Op::Flush => db.flush().unwrap(),
            Op::Compact => db.compact().unwrap(),
            Op::Reopen => {
                drop(std::mem::take(&mut snaps));
                drop(db);
                db = Db::open_with_env(env.clone(), dir, tiny()).unwrap();
            }
            Op::TakeSnapshot => {
                if snaps.len() < 4 {
                    snaps.push((db.snapshot(), model.clone()));
                }
            }
            Op::CheckSnapshot(i) => {
                if let Some((s, m)) = snaps.get(*i) {
                    let got: Vec<_> = db
                        .scan_at(s, None, None)
                        .unwrap()
                        .map(|r| r.unwrap())
                        .collect();
                    assert_eq!(got, model_scan(m, None, None), "снимок {i}: просмотр");
                    for a in 0..22 {
                        assert_eq!(
                            db.get_at(s, &k(a)).unwrap(),
                            m.get(&k(a)).cloned(),
                            "снимок {i}: get {a}"
                        );
                    }
                }
            }
        }
    }
    let all: Vec<_> = db.scan(None, None).unwrap().map(|r| r.unwrap()).collect();
    same(&all, &model_scan(&model, None, None), "итог");
    // после полного уплотнения содержимое то же
    db.compact().unwrap();
    let all: Vec<_> = db.scan(None, None).unwrap().map(|r| r.unwrap()).collect();
    same(&all, &model_scan(&model, None, None), "итог после compact");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 120, max_shrink_iters: 2000, ..ProptestConfig::default() })]
    #[test]
    fn behaves_like_a_btreemap(ops in prop::collection::vec(op(), 1..120)) {
        run_ops(&ops);
    }
}

#[test]
fn long_run_pushes_data_through_several_levels() {
    let (_m, env) = mem_env();
    let db = Db::open_with_env(env, Path::new("db"), tiny()).unwrap();
    let mut model = Model::new();
    let mut x = 12345u64;
    for i in 0..4000u64 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let key = format!("user{:04}", (x >> 33) % 500).into_bytes();
        if (x >> 20).is_multiple_of(5) {
            db.delete(key.clone()).unwrap();
            model.remove(&key);
        } else {
            let val = format!("value-{i}").into_bytes();
            db.put(key.clone(), val.clone()).unwrap();
            model.insert(key, val);
        }
    }
    let st = db.stats();
    assert!(
        st.files_per_level.iter().skip(2).any(|&n| n > 0),
        "данные должны дойти до уровня 2 и глубже: {:?}",
        st.files_per_level
    );
    assert!(
        st.files_per_level.iter().any(|&n| n >= 2),
        "выходные файлы должны делиться по размеру: {:?}",
        st.files_per_level
    );
    let got: Vec<_> = db.scan(None, None).unwrap().map(|r| r.unwrap()).collect();
    assert_eq!(got, model_scan(&model, None, None));
    for (key, val) in &model {
        assert_eq!(db.get(key).unwrap().as_ref(), Some(val));
    }
    assert_eq!(db.get(b"user9999").unwrap(), None);
}
