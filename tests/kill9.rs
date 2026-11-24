//! Настоящая авария: рабочий процесс убивают (SIGKILL / TerminateProcess) в случайный момент, база открывается заново.
//! Это проверяет путь реального файлового ввода-вывода (не симуляцию), но НЕ потерю питания: кэш ОС при убийстве процесса сохраняется.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use tinylsm::{Db, Options};

fn opts() -> Options {
    Options {
        write_buffer_size: 8 * 1024,
        l0_compaction_trigger: 2,
        level_base_bytes: 24 * 1024,
        target_file_size: 8 * 1024,
        sync: true,
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("tinylsm-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn killed_writer_never_loses_acknowledged_batches() {
    let dir = temp_dir("kill9");
    let mut total_acked = 0u64;
    for round in 0..6u64 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_crash_worker"))
            .arg(&dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("запуск рабочего процесса");
        let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
        let want = 120 + round * 53; // каждый раз убиваем на другой глубине: попадаем в сбросы и уплотнения
        let mut last_ack: Option<u64> = None;
        let mut seen = 0;
        while seen < want {
            let line = lines
                .next()
                .expect("процесс завершился сам")
                .expect("чтение");
            last_ack = Some(
                line.strip_prefix("ACK ")
                    .expect("формат ACK")
                    .parse()
                    .expect("число"),
            );
            seen += 1;
        }
        child.kill().expect("убийство");
        child.wait().expect("ожидание");
        drop(lines);
        let last_ack = last_ack.expect("были подтверждения");
        total_acked += seen;

        let db = Db::open(&dir, opts())
            .unwrap_or_else(|e| panic!("раунд {round}: база не открылась после kill: {e}"));
        let last: u64 = String::from_utf8(db.get(b"last").unwrap().expect("ключ last есть"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            last >= last_ack,
            "раунд {round}: подтверждена запись {last_ack}, а в базе последняя {last}"
        );
        // атомарность пакетов и отсутствие дыр: ровно ключи 0..=last с правильными значениями
        let mut count = 0u64;
        for item in db.scan(Some(b"key"), Some(b"kez")).unwrap() {
            let (k, v) = item.unwrap();
            assert_eq!(
                k,
                format!("key{count:08}").into_bytes(),
                "раунд {round}: дыра или лишний ключ"
            );
            assert_eq!(
                v,
                format!("value-{count}-{}", "z".repeat((count % 200) as usize)).into_bytes()
            );
            count += 1;
        }
        assert_eq!(
            count,
            last + 1,
            "раунд {round}: число ключей не сходится с last"
        );
    }
    assert!(total_acked > 600);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn second_process_cannot_open_a_locked_database() {
    let dir = temp_dir("lock");
    let db = Db::open(&dir, opts()).unwrap();
    db.put("a", "1").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_crash_worker"))
        .arg(&dir)
        .stdout(Stdio::null())
        .output()
        .expect("запуск");
    assert!(
        !out.status.success(),
        "второй процесс не должен открыть занятую базу"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("занята"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(matches!(
        Db::open(&dir, opts()),
        Err(tinylsm::Error::Locked(_))
    ));
    drop(db);
    assert!(Db::open(&dir, opts()).is_ok());
    let _ = std::fs::remove_dir_all(&dir);
}
