//! Сбои: «процесс» умирает на каждой возможной точке записи на диск, затем пропадает питание.
//! После этого база обязана открыться, сохранить всё подтверждённое (fsync) и показать состояние,
//! равное результату некоторого ПРЕФИКСА последовательности записей (пакеты — целиком).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use tinylsm::env::{Env, mem_env};
use tinylsm::{Db, Options, WriteBatch};

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

#[derive(Clone, Debug)]
enum Step {
    Put(Vec<u8>, Vec<u8>, bool),
    Del(Vec<u8>, bool),
    Batch(Vec<(Vec<u8>, Option<Vec<u8>>)>, bool),
    Flush,
    Compact,
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn key(i: u64) -> Vec<u8> {
    format!("k{i:02}").into_bytes()
}

fn small_options() -> Options {
    Options {
        write_buffer_size: 600,
        l0_compaction_trigger: 2,
        level_base_bytes: 1500,
        target_file_size: 500,
        sync: true,
    }
}

fn script(seed: u64, len: usize) -> Vec<Step> {
    let mut r = Rng(seed);
    let mut out = Vec::new();
    for n in 0..len {
        let sync = r.below(3) != 0;
        let step = match r.below(10) {
            0..=3 => Step::Put(
                key(r.below(12)),
                format!("v{n}-{}", "x".repeat(r.below(60) as usize)).into_bytes(),
                sync,
            ),
            4 | 5 => Step::Del(key(r.below(12)), sync),
            6 | 7 => {
                let ops = (0..2 + r.below(3))
                    .map(|_| {
                        let k = key(r.below(12));
                        if r.below(3) == 0 {
                            (k, None)
                        } else {
                            (k, Some(format!("b{n}").into_bytes()))
                        }
                    })
                    .collect();
                Step::Batch(ops, sync)
            }
            8 => Step::Flush,
            _ => Step::Compact,
        };
        out.push(step);
    }
    out
}

/// Состояния модели после каждого числа выполненных записей (индекс = сколько записей применено).
fn models(steps: &[Step]) -> Vec<Model> {
    let mut m = Model::new();
    let mut out = vec![m.clone()];
    for s in steps {
        match s {
            Step::Put(k, v, _) => {
                m.insert(k.clone(), v.clone());
            }
            Step::Del(k, _) => {
                m.remove(k);
            }
            Step::Batch(ops, _) => {
                for (k, v) in ops {
                    match v {
                        Some(v) => m.insert(k.clone(), v.clone()),
                        None => m.remove(k),
                    };
                }
            }
            Step::Flush | Step::Compact => continue,
        }
        out.push(m.clone());
    }
    out
}

struct Outcome {
    /// Число записей, которые обязаны пережить сбой (последняя подтверждённая с fsync и всё до неё).
    must: usize,
    /// Число записей, которые могли быть применены (включая оборванную).
    may: usize,
}

/// Выполняет сценарий до первой ошибки (смерти процесса).
fn run(env: &Arc<dyn Env>, steps: &[Step]) -> Outcome {
    let mut o = Outcome { must: 0, may: 0 };
    let Ok(db) = Db::open_with_env(env.clone(), Path::new("db"), small_options()) else {
        return o;
    };
    for s in steps {
        let res = match s {
            Step::Put(k, v, sync) => {
                o.may += 1;
                let mut b = WriteBatch::new();
                b.put(k.clone(), v.clone());
                db.write_opt(&b, *sync).map(|()| Some(*sync))
            }
            Step::Del(k, sync) => {
                o.may += 1;
                let mut b = WriteBatch::new();
                b.delete(k.clone());
                db.write_opt(&b, *sync).map(|()| Some(*sync))
            }
            Step::Batch(ops, sync) => {
                o.may += 1;
                let mut b = WriteBatch::new();
                for (k, v) in ops {
                    match v {
                        Some(v) => b.put(k.clone(), v.clone()),
                        None => b.delete(k.clone()),
                    };
                }
                db.write_opt(&b, *sync).map(|()| Some(*sync))
            }
            Step::Flush => db.flush().map(|()| None),
            Step::Compact => db.compact().map(|()| None),
        };
        match res {
            Ok(Some(true)) => o.must = o.may,
            Ok(_) => {}
            Err(_) => break,
        }
        // успешный flush/compact тоже делает fsync, но подтверждением записей не считается: учитываем только sync-записи
    }
    o
}

fn contents(db: &Db) -> Model {
    db.scan(None, None).unwrap().map(|r| r.unwrap()).collect()
}

fn check_recovered(env: &Arc<dyn Env>, ms: &[Model], o: &Outcome, label: &str) {
    let db = Db::open_with_env(env.clone(), Path::new("db"), small_options())
        .unwrap_or_else(|e| panic!("{label}: база не открылась после сбоя: {e}"));
    let got = contents(&db);
    let ok = (o.must..=o.may).any(|k| ms[k] == got);
    assert!(
        ok,
        "{label}: состояние не равно никакому префиксу в диапазоне {}..={} (подтверждённые записи потеряны или нарушена атомарность)\nполучено: {got:?}",
        o.must, o.may
    );
    for (k, v) in &got {
        assert_eq!(
            db.get(k).unwrap().as_ref(),
            Some(v),
            "{label}: get расходится со scan"
        );
    }
    // база после восстановления работает
    db.put(b"zz".to_vec(), b"after".to_vec()).unwrap();
    assert_eq!(db.get(b"zz").unwrap().unwrap(), b"after");
    db.flush().unwrap();
    drop(db);
    let db = Db::open_with_env(env.clone(), Path::new("db"), small_options())
        .unwrap_or_else(|e| panic!("{label}: повторное открытие: {e}"));
    assert_eq!(db.get(b"zz").unwrap().unwrap(), b"after");
}

fn total_ops(steps: &[Step]) -> u64 {
    let (m, env) = mem_env();
    run(&env, steps);
    m.ops()
}

#[test]
fn crash_at_every_write_point_keeps_a_consistent_prefix() {
    let mut total_points = 0u64;
    for seed in [1u64, 2, 3] {
        let steps = script(seed, 45);
        let ms = models(&steps);
        let n = total_ops(&steps);
        assert!(
            n > 100,
            "сценарий должен проходить через много точек отказа"
        );
        for point in 1..=n {
            let (m, env) = mem_env();
            m.set_crash_at(Some(point));
            let o = run(&env, &steps);
            assert!(m.is_crashed(), "точка {point} из {n} не сработала");
            m.power_loss(seed * 1_000_003 + point);
            check_recovered(
                &env,
                &ms,
                &o,
                &format!("сценарий {seed}, точка {point}/{n}"),
            );
            total_points += 1;
        }
    }
    assert!(total_points > 500, "проверено точек отказа: {total_points}");
}

#[test]
fn crash_during_recovery_is_also_safe() {
    let steps = script(7, 40);
    let ms = models(&steps);
    let n = total_ops(&steps);
    for (i, point) in (1..=n).step_by(23).enumerate() {
        let (m, env) = mem_env();
        m.set_crash_at(Some(point));
        let o = run(&env, &steps);
        m.power_loss(point + 99);
        // сколько операций делает само восстановление
        m.set_crash_at(None);
        let before = m.ops();
        drop(Db::open_with_env(env.clone(), Path::new("db"), small_options()).unwrap());
        let recovery_ops = m.ops() - before;
        // повторяем: снова та же авария, потеря питания, и теперь умираем на каждой точке восстановления
        for rp in 1..=recovery_ops {
            let (m2, env2) = mem_env();
            m2.set_crash_at(Some(point));
            let o2 = run(&env2, &steps);
            m2.power_loss(point + 99);
            m2.set_crash_at(Some(rp));
            let _ = Db::open_with_env(env2.clone(), Path::new("db"), small_options());
            m2.power_loss(rp * 31 + i as u64);
            check_recovered(
                &env2,
                &ms,
                &o2,
                &format!("авария {point}, смерть в восстановлении на точке {rp}/{recovery_ops}"),
            );
        }
        let _ = (o, &ms);
    }
}

#[test]
fn unsynced_writes_may_vanish_but_synced_ones_never_do() {
    for seed in 0..60 {
        let (m, env) = mem_env();
        let db = Db::open_with_env(env.clone(), Path::new("db"), Options::default()).unwrap();
        let mut b = WriteBatch::new();
        b.put("a", "1");
        db.write_opt(&b, true).unwrap();
        for i in 0..50 {
            let mut b = WriteBatch::new();
            b.put(format!("u{i}"), "x");
            db.write_opt(&b, false).unwrap();
        }
        drop(db);
        m.power_loss(seed);
        let db = Db::open_with_env(env.clone(), Path::new("db"), Options::default()).unwrap();
        assert_eq!(
            db.get(b"a").unwrap().unwrap(),
            b"1",
            "подтверждённая запись потеряна (seed {seed})"
        );
        // несинхронизированные записи сохраняются префиксом: если есть u{i}, то есть и все u{j<i}
        let present: Vec<bool> = (0..50)
            .map(|i| db.get(format!("u{i}").as_bytes()).unwrap().is_some())
            .collect();
        let first_missing = present.iter().position(|p| !p).unwrap_or(50);
        assert!(
            present[first_missing..].iter().all(|p| !p),
            "дыра в несинхронизированных записях: {present:?}"
        );
    }
}
