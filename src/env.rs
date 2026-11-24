//! Абстракция файловой системы: настоящая (`RealEnv`) и симулированная с потерей питания (`MemEnv`).
//!
//! Весь код базы работает только через `Env`. Это позволяет тестам остановить «процесс» на любой операции записи,
//! сбросить всё, что не было синхронизировано, и проверить, что база открывается и ничего подтверждённого не потеряла.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

/// Файл для последовательной записи (журнал, SSTable, манифест).
pub trait WritableFile: Send {
    fn append(&mut self, data: &[u8]) -> io::Result<()>;
    /// Сбросить записанное на устойчивый носитель (fsync).
    fn sync(&mut self) -> io::Result<()>;
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Файл для чтения по смещению (SSTable).
pub trait RandomAccessFile: Send + Sync {
    /// Читает до `len` байт с `offset`; короче, только если файл закончился.
    fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>>;
    fn len(&self) -> u64;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub trait Env: Send + Sync + 'static {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()>;
    fn exists(&self, path: &Path) -> bool;
    /// Имена (без пути) файлов каталога.
    fn list(&self, dir: &Path) -> io::Result<Vec<String>>;
    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>>;
    /// Создаёт файл (с усечением, если он есть).
    fn create_file(&self, path: &Path) -> io::Result<Box<dyn WritableFile>>;
    fn open_random(&self, path: &Path) -> io::Result<Arc<dyn RandomAccessFile>>;
    /// Атомарно переименовывает; существующий `to` заменяется.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn remove(&self, path: &Path) -> io::Result<()>;
    /// Делает устойчивыми создание, переименование и удаление файлов в каталоге.
    fn sync_dir(&self, dir: &Path) -> io::Result<()>;
    /// Исключительная блокировка (одна база: один владелец). Блокировка снимается, когда значение отброшено.
    fn lock(&self, path: &Path) -> io::Result<Box<dyn Send>>;
}

// ---------- настоящая файловая система ----------

#[derive(Default, Clone, Copy)]
pub struct RealEnv;

struct RealWritable {
    file: File,
    len: u64,
}

impl WritableFile for RealWritable {
    fn append(&mut self, data: &[u8]) -> io::Result<()> {
        self.file.write_all(data)?;
        self.len += data.len() as u64;
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }

    fn len(&self) -> u64 {
        self.len
    }
}

struct RealRandom {
    file: File,
    len: u64,
}

impl RandomAccessFile for RealRandom {
    fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let mut done = 0;
        while done < len {
            let n = read_at_impl(&self.file, &mut buf[done..], offset + done as u64)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        buf.truncate(done);
        Ok(buf)
    }

    fn len(&self) -> u64 {
        self.len
    }
}

#[cfg(unix)]
fn read_at_impl(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
fn read_at_impl(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

impl Env for RealEnv {
    fn create_dir_all(&self, dir: &Path) -> io::Result<()> {
        fs::create_dir_all(dir)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for e in fs::read_dir(dir)? {
            out.push(e?.file_name().to_string_lossy().into_owned());
        }
        Ok(out)
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn create_file(&self, path: &Path) -> io::Result<Box<dyn WritableFile>> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        Ok(Box::new(RealWritable { file, len: 0 }))
    }

    fn open_random(&self, path: &Path) -> io::Result<Arc<dyn RandomAccessFile>> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        Ok(Arc::new(RealRandom { file, len }))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        fs::rename(from, to)
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    #[cfg(unix)]
    fn sync_dir(&self, dir: &Path) -> io::Result<()> {
        File::open(dir)?.sync_all()
    }

    #[cfg(not(unix))]
    fn sync_dir(&self, _dir: &Path) -> io::Result<()> {
        // NTFS ведёт журнал изменений каталога сам; открыть каталог для fsync через std нельзя
        Ok(())
    }

    fn lock(&self, path: &Path) -> io::Result<Box<dyn Send>> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Box::new(file)),
            Err(fs::TryLockError::WouldBlock) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "файл LOCK занят другим процессом",
            )),
            Err(fs::TryLockError::Error(e)) => Err(e),
        }
    }
}

// ---------- симулированная файловая система ----------

#[derive(Clone)]
enum NsOp {
    Create(String, u64),
    Rename(String, String),
    Remove(String),
}

struct Inode {
    data: Vec<u8>,
    /// Сколько байт гарантированно пережило бы потерю питания (граница последнего fsync).
    synced: usize,
}

struct MemState {
    next_ino: u64,
    inodes: HashMap<u64, Inode>,
    names: HashMap<String, u64>,
    durable_names: HashMap<String, u64>,
    journal: Vec<NsOp>,
    ops: u64,
    crash_at: Option<u64>,
    crashed: bool,
    locks: HashSet<String>,
}

/// Файловая система в памяти, моделирующая потерю питания.
///
/// Мутирующие операции (создание, дозапись, fsync, переименование, удаление, fsync каталога) считаются «точками отказа»:
/// на выбранной точке `MemEnv` бросает ошибку и «умирает» (все дальнейшие обращения тоже падают), как процесс, которого убили.
/// `power_loss` затем строит то состояние диска, которое могло остаться: несинхронизированные хвосты файлов обрезаются
/// (иногда с мусором в последних байтах), а изменения имён после последнего fsync каталога сохраняются лишь частично.
pub struct MemEnv {
    state: Mutex<MemState>,
}

struct SplitMix(u64);

impl SplitMix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() % n as u64) as usize
        }
    }
}

fn norm(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn parent_of(name: &str) -> &str {
    name.rsplit_once('/').map_or("", |(p, _)| p)
}

fn injected() -> io::Error {
    io::Error::other("инъекция сбоя: процесс остановлен")
}

impl MemEnv {
    pub fn new() -> Arc<MemEnv> {
        Arc::new(MemEnv {
            state: Mutex::new(MemState {
                next_ino: 1,
                inodes: HashMap::new(),
                names: HashMap::new(),
                durable_names: HashMap::new(),
                journal: Vec::new(),
                ops: 0,
                crash_at: None,
                crashed: false,
                locks: HashSet::new(),
            }),
        })
    }

    fn st(&self) -> MutexGuard<'_, MemState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Номер следующей точки отказа, на которой «процесс умрёт» (None: не падать).
    pub fn set_crash_at(&self, op: Option<u64>) {
        self.st().crash_at = op;
    }

    /// Сколько мутирующих операций выполнено с начала (или с последнего `power_loss`).
    pub fn ops(&self) -> u64 {
        self.st().ops
    }

    pub fn is_crashed(&self) -> bool {
        self.st().crashed
    }

    /// Содержимое файла (для тестов повреждений).
    pub fn contents(&self, path: &Path) -> Option<Vec<u8>> {
        let s = self.st();
        s.names
            .get(&norm(path))
            .and_then(|i| s.inodes.get(i))
            .map(|n| n.data.clone())
    }

    /// Портит один байт файла (имитация порчи носителя).
    pub fn corrupt_byte(&self, path: &Path, offset: usize, xor: u8) {
        let mut s = self.st();
        let ino = s.names[&norm(path)];
        let data = &mut s.inodes.get_mut(&ino).expect("инод есть").data;
        data[offset] ^= xor;
    }

    /// Обрезает файл до `len` байт (имитация оборванной записи).
    pub fn truncate(&self, path: &Path, len: usize) {
        let mut s = self.st();
        let ino = s.names[&norm(path)];
        let node = s.inodes.get_mut(&ino).expect("инод есть");
        node.data.truncate(len);
        node.synced = node.synced.min(len);
    }

    /// Потеря питания: остаётся то, что могло дожить. Дальше `MemEnv` снова работоспособен (перезагрузка).
    pub fn power_loss(&self, seed: u64) {
        let mut rng = SplitMix(seed ^ 0xA5A5_5A5A_1234_5678);
        let mut s = self.st();
        let mut names = s.durable_names.clone();
        let applied = rng.below(s.journal.len() + 1);
        for op in s.journal.clone().into_iter().take(applied) {
            match op {
                NsOp::Create(n, i) => {
                    names.insert(n, i);
                }
                NsOp::Rename(a, b) => {
                    if let Some(i) = names.remove(&a) {
                        names.insert(b, i);
                    }
                }
                NsOp::Remove(n) => {
                    names.remove(&n);
                }
            }
        }
        let live: HashSet<u64> = names.values().copied().collect();
        s.inodes.retain(|i, _| live.contains(i));
        for node in s.inodes.values_mut() {
            let unsynced = node.data.len() - node.synced;
            let keep = node.synced
                + match rng.below(4) {
                    0 => 0,
                    1 => unsynced,
                    _ => rng.below(unsynced + 1),
                };
            node.data.truncate(keep);
            if keep > node.synced && rng.below(5) == 0 {
                let at = node.synced + rng.below(keep - node.synced);
                // последний «записанный» кусок мог оказаться испорченным; после него на диске ничего нет
                node.data[at] ^= (rng.below(255) + 1) as u8;
                node.data.truncate(at + 1);
            }
            node.synced = node.data.len();
        }
        s.durable_names = names.clone();
        s.names = names;
        s.journal.clear();
        s.ops = 0;
        s.crash_at = None;
        s.crashed = false;
        s.locks.clear();
    }
}

impl MemState {
    fn tick(&mut self) -> io::Result<()> {
        if self.crashed {
            return Err(injected());
        }
        self.ops += 1;
        if self.crash_at == Some(self.ops) {
            self.crashed = true;
            return Err(injected());
        }
        Ok(())
    }

    fn alive(&self) -> io::Result<()> {
        if self.crashed {
            Err(injected())
        } else {
            Ok(())
        }
    }
}

struct MemWritable {
    env: Arc<MemEnv>,
    ino: u64,
}

impl WritableFile for MemWritable {
    fn append(&mut self, data: &[u8]) -> io::Result<()> {
        let mut s = self.env.st();
        s.tick()?;
        s.inodes
            .get_mut(&self.ino)
            .ok_or_else(|| io::Error::other("файл удалён"))?
            .data
            .extend_from_slice(data);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let mut s = self.env.st();
        s.tick()?;
        let node = s
            .inodes
            .get_mut(&self.ino)
            .ok_or_else(|| io::Error::other("файл удалён"))?;
        node.synced = node.data.len();
        Ok(())
    }

    fn len(&self) -> u64 {
        self.env
            .st()
            .inodes
            .get(&self.ino)
            .map_or(0, |n| n.data.len() as u64)
    }
}

struct MemRandom {
    env: Arc<MemEnv>,
    ino: u64,
}

impl RandomAccessFile for MemRandom {
    fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let s = self.env.st();
        s.alive()?;
        let node = s
            .inodes
            .get(&self.ino)
            .ok_or_else(|| io::Error::other("файл удалён"))?;
        let start = (offset as usize).min(node.data.len());
        let end = (start + len).min(node.data.len());
        Ok(node.data[start..end].to_vec())
    }

    fn len(&self) -> u64 {
        self.env
            .st()
            .inodes
            .get(&self.ino)
            .map_or(0, |n| n.data.len() as u64)
    }
}

struct MemLock {
    env: Arc<MemEnv>,
    name: String,
}

impl Drop for MemLock {
    fn drop(&mut self) {
        self.env.st().locks.remove(&self.name);
    }
}

/// `Env` поверх `Arc<MemEnv>`: нужен, потому что дескрипторы файлов хранят ссылку на окружение.
pub struct MemEnvHandle(pub Arc<MemEnv>);

impl Env for MemEnvHandle {
    fn create_dir_all(&self, _dir: &Path) -> io::Result<()> {
        self.0.st().alive()
    }

    fn exists(&self, path: &Path) -> bool {
        self.0.st().names.contains_key(&norm(path))
    }

    fn list(&self, dir: &Path) -> io::Result<Vec<String>> {
        let s = self.0.st();
        s.alive()?;
        let d = norm(dir);
        Ok(s.names
            .keys()
            .filter(|n| parent_of(n) == d)
            .map(|n| n[d.len() + 1..].to_string())
            .collect())
    }

    fn read_file(&self, path: &Path) -> io::Result<Vec<u8>> {
        let s = self.0.st();
        s.alive()?;
        let ino = s
            .names
            .get(&norm(path))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "нет файла"))?;
        Ok(s.inodes[ino].data.clone())
    }

    fn create_file(&self, path: &Path) -> io::Result<Box<dyn WritableFile>> {
        let mut s = self.0.st();
        s.tick()?;
        let name = norm(path);
        let ino = s.next_ino;
        s.next_ino += 1;
        s.inodes.insert(
            ino,
            Inode {
                data: Vec::new(),
                synced: 0,
            },
        );
        s.names.insert(name.clone(), ino);
        s.journal.push(NsOp::Create(name, ino));
        Ok(Box::new(MemWritable {
            env: self.0.clone(),
            ino,
        }))
    }

    fn open_random(&self, path: &Path) -> io::Result<Arc<dyn RandomAccessFile>> {
        let s = self.0.st();
        s.alive()?;
        let ino = *s
            .names
            .get(&norm(path))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "нет файла"))?;
        Ok(Arc::new(MemRandom {
            env: self.0.clone(),
            ino,
        }))
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut s = self.0.st();
        s.tick()?;
        let (a, b) = (norm(from), norm(to));
        let ino = s
            .names
            .remove(&a)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "нет файла"))?;
        s.names.insert(b.clone(), ino);
        s.journal.push(NsOp::Rename(a, b));
        Ok(())
    }

    fn remove(&self, path: &Path) -> io::Result<()> {
        let mut s = self.0.st();
        s.tick()?;
        let name = norm(path);
        s.names
            .remove(&name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "нет файла"))?;
        s.journal.push(NsOp::Remove(name));
        Ok(())
    }

    fn sync_dir(&self, _dir: &Path) -> io::Result<()> {
        let mut s = self.0.st();
        s.tick()?;
        s.durable_names = s.names.clone();
        s.journal.clear();
        Ok(())
    }

    fn lock(&self, path: &Path) -> io::Result<Box<dyn Send>> {
        let mut s = self.0.st();
        s.alive()?;
        let name = norm(path);
        if !s.locks.insert(name.clone()) {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "файл LOCK занят"));
        }
        Ok(Box::new(MemLock {
            env: self.0.clone(),
            name,
        }))
    }
}

/// Удобный конструктор: окружение в памяти и `Env` для базы.
pub fn mem_env() -> (Arc<MemEnv>, Arc<dyn Env>) {
    let m = MemEnv::new();
    (m.clone(), Arc::new(MemEnvHandle(m)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn mem_env_basic_io() {
        let (m, env) = mem_env();
        let mut f = env.create_file(&p("d/a")).unwrap();
        f.append(b"hello").unwrap();
        f.append(b" world").unwrap();
        assert_eq!(f.len(), 11);
        assert_eq!(env.read_file(&p("d/a")).unwrap(), b"hello world");
        let r = env.open_random(&p("d/a")).unwrap();
        assert_eq!(r.read_at(6, 100).unwrap(), b"world");
        assert_eq!(r.read_at(100, 5).unwrap(), b"");
        env.rename(&p("d/a"), &p("d/b")).unwrap();
        assert!(!env.exists(&p("d/a")) && env.exists(&p("d/b")));
        assert_eq!(env.list(&p("d")).unwrap(), vec!["b".to_string()]);
        env.remove(&p("d/b")).unwrap();
        assert!(env.read_file(&p("d/b")).is_err());
        assert!(m.ops() >= 5);
    }

    #[test]
    fn unsynced_data_may_be_lost_but_synced_data_never_is() {
        for seed in 0..200 {
            let (m, env) = mem_env();
            let mut f = env.create_file(&p("d/log")).unwrap();
            f.append(b"durable").unwrap();
            f.sync().unwrap();
            env.sync_dir(&p("d")).unwrap();
            f.append(b"-maybe-lost").unwrap();
            m.power_loss(seed);
            let data = env.read_file(&p("d/log")).unwrap();
            assert!(
                data.starts_with(b"durable") || data.len() >= 7,
                "синхронизированное не должно теряться"
            );
            assert!(data.len() >= 7 && data.len() <= 18);
            assert_eq!(&data[..7], b"durable");
        }
    }

    #[test]
    fn file_without_directory_sync_may_vanish() {
        let mut vanished = 0;
        let mut kept = 0;
        for seed in 0..200 {
            let (m, env) = mem_env();
            let mut f = env.create_file(&p("d/new")).unwrap();
            f.append(b"x").unwrap();
            f.sync().unwrap(); // содержимое синхронизировано, а имя файла в каталоге ещё нет
            m.power_loss(seed);
            if env.exists(&p("d/new")) {
                kept += 1;
            } else {
                vanished += 1;
            }
        }
        assert!(
            vanished > 20 && kept > 20,
            "модель должна давать оба исхода: {vanished}/{kept}"
        );
    }

    #[test]
    fn rename_is_durable_only_after_sync_dir() {
        for seed in 0..100 {
            let (m, env) = mem_env();
            let mut f = env.create_file(&p("d/tmp")).unwrap();
            f.append(b"v").unwrap();
            f.sync().unwrap();
            env.sync_dir(&p("d")).unwrap();
            env.rename(&p("d/tmp"), &p("d/final")).unwrap();
            env.sync_dir(&p("d")).unwrap();
            m.power_loss(seed);
            assert!(
                env.exists(&p("d/final")) && !env.exists(&p("d/tmp")),
                "после sync_dir переименование обязано пережить сбой"
            );
        }
    }

    #[test]
    fn crash_point_stops_the_process_and_power_loss_revives_it() {
        let (m, env) = mem_env();
        m.set_crash_at(Some(3));
        let mut f = env.create_file(&p("d/x")).unwrap(); // операция 1
        f.append(b"a").unwrap(); // 2
        assert!(f.append(b"b").is_err()); // 3: здесь процесс «умер»
        assert!(m.is_crashed());
        assert!(f.sync().is_err() && env.read_file(&p("d/x")).is_err());
        m.power_loss(1);
        assert!(!m.is_crashed());
        let _ = env.create_file(&p("d/y")).unwrap();
    }

    #[test]
    fn locks_are_exclusive() {
        let (_m, env) = mem_env();
        let g = env.lock(&p("d/LOCK")).unwrap();
        assert!(env.lock(&p("d/LOCK")).is_err());
        drop(g);
        assert!(env.lock(&p("d/LOCK")).is_ok());
    }

    #[test]
    fn real_env_roundtrip_and_lock() {
        let dir = std::env::temp_dir().join(format!("tinylsm-env-{}", std::process::id()));
        let env = RealEnv;
        env.create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        let mut f = env.create_file(&path).unwrap();
        f.append(b"0123456789").unwrap();
        f.sync().unwrap();
        env.sync_dir(&dir).unwrap();
        let r = env.open_random(&path).unwrap();
        assert_eq!(r.read_at(3, 4).unwrap(), b"3456");
        assert_eq!(r.read_at(8, 10).unwrap(), b"89");
        let l1 = env.lock(&dir.join("LOCK")).unwrap();
        assert!(env.lock(&dir.join("LOCK")).is_err());
        drop(l1);
        drop(r);
        drop(f);
        env.remove(&path).unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
