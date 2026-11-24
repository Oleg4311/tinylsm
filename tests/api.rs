//! Прикладные проверки на настоящей файловой системе: CLI, повреждения файлов, ограничения.

use std::path::PathBuf;
use std::process::Command;

use tinylsm::{Db, Error, Options, WriteBatch};

fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("tinylsm-api-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn cli(dir: &PathBuf, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_tinylsm"))
        .arg(dir)
        .args(args)
        .output()
        .expect("запуск tinylsm");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
        String::from_utf8_lossy(&out.stderr).trim().to_string(),
    )
}

#[test]
fn cli_roundtrip() {
    let dir = temp_dir("cli");
    assert!(cli(&dir, &["put", "привет", "мир"]).0);
    assert_eq!(cli(&dir, &["get", "привет"]).1, "мир");
    let missing = cli(&dir, &["get", "нет"]);
    assert!(!missing.0 && missing.2.contains("нет"));
    assert!(cli(&dir, &["fill", "2500"]).0);
    let scan = cli(&dir, &["scan", "key000010", "key000013"]).1;
    assert_eq!(
        scan.lines().collect::<Vec<_>>(),
        vec![
            "key000010\tvalue-10",
            "key000011\tvalue-11",
            "key000012\tvalue-12"
        ]
    );
    assert!(cli(&dir, &["del", "key000011"]).0);
    assert!(!cli(&dir, &["get", "key000011"]).0);
    assert!(cli(&dir, &["compact"]).0);
    assert!(cli(&dir, &["stats"]).1.contains("L"));
    assert!(cli(&dir, &["verify"]).1.contains("ключей: 2500")); // 2500 + «привет» − удалённый
    assert!(!cli(&dir, &["unknown"]).0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn flipped_byte_in_a_table_is_reported_not_returned() {
    let dir = temp_dir("corrupt");
    {
        let db = Db::open(
            &dir,
            Options {
                write_buffer_size: 1 << 20,
                ..Options::default()
            },
        )
        .unwrap();
        for i in 0..500 {
            db.put(format!("k{i:04}"), format!("v{i}")).unwrap();
        }
        db.flush().unwrap();
    }
    let sst = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "sst"))
        .expect("есть SST");
    let mut data = std::fs::read(&sst).unwrap();
    data[40] ^= 0xFF; // внутри первого блока данных
    std::fs::write(&sst, &data).unwrap();
    let db = Db::open(&dir, Options::default())
        .expect("повреждён блок данных, а не подвал: открытие возможно");
    let err = db
        .get(b"k0001")
        .expect_err("порча должна обнаруживаться при чтении");
    assert!(matches!(err, Error::Corruption(_)), "{err}");
    // а порча подвала мешает открытию
    let mut data = std::fs::read(&sst).unwrap();
    let n = data.len();
    data[n - 10] ^= 0xFF;
    std::fs::write(&sst, &data).unwrap();
    drop(db);
    assert!(matches!(
        Db::open(&dir, Options::default()),
        Err(Error::Corruption(_))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn damaged_log_in_the_middle_is_an_error_but_a_torn_tail_is_not() {
    let dir = temp_dir("wal");
    {
        let db = Db::open(&dir, Options::default()).unwrap();
        for i in 0..20 {
            db.put(format!("k{i}"), "value").unwrap();
        }
    }
    let wal = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "wal"))
        .unwrap();
    let good = std::fs::read(&wal).unwrap();
    // оборванный хвост: последняя запись недописана
    std::fs::write(&wal, &good[..good.len() - 3]).unwrap();
    let db = Db::open(&dir, Options::default()).unwrap();
    assert_eq!(db.get(b"k0").unwrap().unwrap(), b"value");
    assert_eq!(db.get(b"k19").unwrap(), None);
    assert_eq!(db.get(b"k18").unwrap().unwrap(), b"value");
    drop(db);
    // порча в середине: открываться нельзя, иначе молча потеряем подтверждённые записи
    let wal = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "wal"))
        .unwrap();
    std::fs::write(&wal, &good).unwrap();
    let mut bad = good.clone();
    bad[12] ^= 0xFF;
    std::fs::write(&wal, &bad).unwrap();
    assert!(matches!(
        Db::open(&dir, Options::default()),
        Err(Error::Corruption(_))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn invalid_arguments_are_rejected() {
    let dir = temp_dir("args");
    let db = Db::open(&dir, Options::default()).unwrap();
    assert!(matches!(db.put("", "x"), Err(Error::InvalidArgument(_))));
    assert!(matches!(
        db.put(vec![0u8; 70_000], "x"),
        Err(Error::InvalidArgument(_))
    ));
    assert!(db.write(&WriteBatch::new()).is_ok());
    db.put("k", "").unwrap();
    assert_eq!(
        db.get(b"k").unwrap().unwrap(),
        b"",
        "пустое значение допустимо и отличается от отсутствия"
    );
    assert_eq!(db.get(b"nope").unwrap(), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn snapshot_keeps_old_view_across_flush_and_compaction() {
    let dir = temp_dir("snap");
    let db = Db::open(
        &dir,
        Options {
            write_buffer_size: 512,
            l0_compaction_trigger: 2,
            ..Options::default()
        },
    )
    .unwrap();
    db.put("a", "old").unwrap();
    db.put("gone", "x").unwrap();
    let snap = db.snapshot();
    db.put("a", "new").unwrap();
    db.delete("gone").unwrap();
    for i in 0..300 {
        db.put(format!("filler{i}"), "y".repeat(40)).unwrap();
    }
    db.compact().unwrap();
    assert_eq!(db.get(b"a").unwrap().unwrap(), b"new");
    assert_eq!(db.get_at(&snap, b"a").unwrap().unwrap(), b"old");
    assert_eq!(db.get_at(&snap, b"gone").unwrap().unwrap(), b"x");
    assert_eq!(db.get(b"gone").unwrap(), None);
    assert_eq!(db.stats().live_snapshots, 1);
    drop(snap);
    assert_eq!(db.stats().live_snapshots, 0);
    db.compact().unwrap();
    assert_eq!(db.get(b"a").unwrap().unwrap(), b"new");
    let _ = std::fs::remove_dir_all(&dir);
}
