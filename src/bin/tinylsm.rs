//! Командная строка для работы с базой tinylsm.

use std::process::ExitCode;

use tinylsm::{Db, Options, WriteBatch};

const HELP: &str = "tinylsm — встраиваемое LSM-хранилище «ключ-значение»

Использование: tinylsm <каталог> <команда> [аргументы]

Команды:
  put <ключ> <значение>     записать значение
  get <ключ>                прочитать значение (код выхода 1, если ключа нет)
  del <ключ>                удалить ключ
  scan [от [до]]            вывести пары из диапазона [от, до)
  fill <N>                  записать N тестовых пар (key000000…) одним пакетом на 1000
  stats                     файлы и размеры по уровням
  compact                   сбросить память на диск и слить уровни
  verify                    прочитать каждую запись и проверить все контрольные суммы

Ключи и значения — обычный текст (UTF-8).";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if args.len() < 2 {
        eprintln!("{HELP}");
        return ExitCode::from(2);
    }
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("ошибка: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> tinylsm::Result<ExitCode> {
    let db = Db::open(&args[0], Options::default())?;
    let rest: Vec<&str> = args[2..].iter().map(String::as_str).collect();
    match (args[1].as_str(), rest.as_slice()) {
        ("put", [k, v]) => db.put(*k, *v)?,
        ("get", [k]) => match db.get(k.as_bytes())? {
            Some(v) => println!("{}", String::from_utf8_lossy(&v)),
            None => {
                eprintln!("ключа нет");
                return Ok(ExitCode::from(1));
            }
        },
        ("del", [k]) => db.delete(*k)?,
        ("scan", r) if r.len() <= 2 => {
            for item in db.scan(
                r.first().map(|s| s.as_bytes()),
                r.get(1).map(|s| s.as_bytes()),
            )? {
                let (k, v) = item?;
                println!(
                    "{}\t{}",
                    String::from_utf8_lossy(&k),
                    String::from_utf8_lossy(&v)
                );
            }
        }
        ("fill", [n]) => {
            let n: u64 = n
                .parse()
                .map_err(|_| tinylsm::Error::InvalidArgument("N должно быть числом".into()))?;
            let mut i = 0;
            while i < n {
                let mut b = WriteBatch::new();
                for j in i..(i + 1000).min(n) {
                    b.put(format!("key{j:06}"), format!("value-{j}"));
                }
                db.write(&b)?;
                i += 1000;
            }
            println!("записано {n}");
        }
        ("stats", []) => {
            let s = db.stats();
            println!("последний номер записи: {}", s.last_seq);
            println!("memtable: {} байт", s.memtable_bytes);
            for (l, (n, b)) in s.files_per_level.iter().zip(&s.bytes_per_level).enumerate() {
                if *n > 0 {
                    println!("L{l}: файлов {n}, {b} байт");
                }
            }
        }
        ("compact", []) => {
            db.compact()?;
            println!("готово");
        }
        ("verify", []) => {
            // полный просмотр читает и проверяет каждый блок каждого файла
            let mut n = 0u64;
            for item in db.scan(None, None)? {
                item?;
                n += 1;
            }
            println!("проверено, живых ключей: {n}");
        }
        _ => {
            eprintln!("{HELP}");
            return Ok(ExitCode::from(2));
        }
    }
    Ok(ExitCode::SUCCESS)
}
