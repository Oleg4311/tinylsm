//! Рабочий процесс для проверки настоящего аварийного завершения (`kill -9`): пишет пакеты и печатает «ACK i»
//! после каждой подтверждённой (fsync) записи. Тест убивает процесс в случайный момент и проверяет базу.
//!
//! Запуск вручную: `crash_worker <каталог>`.

use std::io::Write;

use tinylsm::{Db, Options, WriteBatch};

fn main() {
    let Some(dir) = std::env::args().nth(1) else {
        eprintln!("использование: crash_worker <каталог>");
        std::process::exit(2);
    };
    let opts = Options {
        write_buffer_size: 8 * 1024,
        l0_compaction_trigger: 2,
        level_base_bytes: 24 * 1024,
        target_file_size: 8 * 1024,
        sync: true,
    };
    let db = match Db::open(&dir, opts) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("не удалось открыть базу: {e}");
            std::process::exit(1);
        }
    };
    let start = match db.get(b"last") {
        Ok(Some(v)) => String::from_utf8_lossy(&v)
            .parse::<u64>()
            .map_or(0, |n| n + 1),
        Ok(None) => 0,
        Err(e) => {
            eprintln!("чтение не удалось: {e}");
            std::process::exit(1);
        }
    };
    let stdout = std::io::stdout();
    for i in start.. {
        let mut b = WriteBatch::new();
        b.put(
            format!("key{i:08}"),
            format!("value-{i}-{}", "z".repeat((i % 200) as usize)),
        );
        b.put("last", i.to_string());
        if let Err(e) = db.write(&b) {
            eprintln!("запись {i} не удалась: {e}");
            std::process::exit(1);
        }
        {
            let mut out = stdout.lock();
            let _ = writeln!(out, "ACK {i}");
            let _ = out.flush();
        }
        if i % 150 == 149
            && let Err(e) = db.compact()
        {
            eprintln!("уплотнение не удалось: {e}");
            std::process::exit(1);
        }
    }
}
