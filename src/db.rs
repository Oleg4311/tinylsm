//! База данных: запись через WAL и memtable, сброс на диск, уплотнение по уровням, снимки, просмотр диапазонов, восстановление.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::batch::{Kind, WriteBatch};
use crate::env::{Env, RealEnv};
use crate::error::{Error, Result, corruption};
use crate::memtable::MemTable;
use crate::sstable::{Entry, Table, TableBuilder, TableIter, cmp_internal};
use crate::version::{FileMeta, FileSpec, ManifestState, NUM_LEVELS, Version, VersionEdit};
use crate::wal::{self, WalWriter};

/// Параметры базы. Значения по умолчанию подходят для небольших наборов данных.
#[derive(Clone, Debug)]
pub struct Options {
    /// Размер memtable, при котором она сбрасывается в файл уровня 0.
    pub write_buffer_size: usize,
    /// Сколько файлов на уровне 0 запускают уплотнение.
    pub l0_compaction_trigger: usize,
    /// Предельный размер уровня 1; каждый следующий уровень в 10 раз больше.
    pub level_base_bytes: u64,
    /// Желаемый размер файла при уплотнении.
    pub target_file_size: u64,
    /// Делать ли fsync журнала при каждой записи по умолчанию.
    pub sync: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            write_buffer_size: 4 << 20,
            l0_compaction_trigger: 4,
            level_base_bytes: 10 << 20,
            target_file_size: 2 << 20,
            sync: true,
        }
    }
}

/// Сводка о состоянии базы.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stats {
    pub files_per_level: Vec<usize>,
    pub bytes_per_level: Vec<u64>,
    pub memtable_bytes: usize,
    pub last_seq: u64,
    pub live_snapshots: usize,
}

struct State {
    mem: MemTable,
    wal: WalWriter,
    wal_number: u64,
    manifest: WalWriter,
    version: Arc<Version>,
    next_file: u64,
    last_seq: u64,
    snapshots: BTreeMap<u64, usize>,
    compact_pointer: Vec<Vec<u8>>,
    poisoned: Option<String>,
    _lock: Box<dyn Send>,
}

struct Inner {
    env: Arc<dyn Env>,
    dir: PathBuf,
    opts: Options,
    state: Mutex<State>,
}

/// Открытая база. Дёшево клонируется; все клоны работают с одной базой.
#[derive(Clone)]
pub struct Db {
    inner: Arc<Inner>,
}

/// Зафиксированный момент времени: чтения через снимок не видят более поздних записей.
pub struct Snapshot {
    inner: Arc<Inner>,
    seq: u64,
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let mut st = self.inner.lock();
        if let Some(c) = st.snapshots.get_mut(&self.seq) {
            *c -= 1;
            if *c == 0 {
                st.snapshots.remove(&self.seq);
            }
        }
    }
}

fn wal_path(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("{n:06}.wal"))
}
fn sst_path(dir: &Path, n: u64) -> PathBuf {
    dir.join(format!("{n:06}.sst"))
}
fn manifest_name(n: u64) -> String {
    format!("MANIFEST-{n:06}")
}

fn parse_number(name: &str, suffix: &str) -> Option<u64> {
    name.strip_suffix(suffix)?.parse().ok()
}

fn failed_error(msg: &str) -> Error {
    Error::Io(io::Error::other(format!(
        "база в аварийном состоянии ({msg}); закройте и откройте её заново"
    )))
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn open_meta(&self, spec: FileSpec) -> Result<Arc<FileMeta>> {
        open_meta(&*self.env, &self.dir, spec)
    }

    fn alloc(st: &mut State) -> u64 {
        let n = st.next_file;
        st.next_file += 1;
        n
    }

    /// Записывает правку в манифест и сбрасывает её на диск.
    fn log_edit(&self, st: &mut State, edit: &VersionEdit) -> Result<()> {
        st.manifest.add_record(&edit.encode(), true)?;
        Ok(())
    }

    fn poison_on_err<T>(&self, st: &mut State, r: Result<T>) -> Result<T> {
        if let Err(e) = &r {
            st.poisoned = Some(e.to_string());
        }
        r
    }

    fn check(st: &State) -> Result<()> {
        match &st.poisoned {
            Some(m) => Err(failed_error(m)),
            None => Ok(()),
        }
    }

    /// Сбрасывает memtable в новый файл уровня 0 и переключает журнал.
    fn flush_locked(&self, st: &mut State) -> Result<()> {
        if st.mem.is_empty() {
            return Ok(());
        }
        let r = self.flush_inner(st);
        self.poison_on_err(st, r)
    }

    fn flush_inner(&self, st: &mut State) -> Result<()> {
        let number = Inner::alloc(st);
        let spec = write_mem_table(&*self.env, &self.dir, number, &st.mem)?;
        let new_wal = Inner::alloc(st);
        let wal_file = self.env.create_file(&wal_path(&self.dir, new_wal))?;
        self.env.sync_dir(&self.dir)?;
        let meta = self.open_meta(spec.clone())?;
        let edit = VersionEdit {
            log_number: new_wal,
            next_file: st.next_file,
            last_seq: st.last_seq,
            deleted: vec![],
            added: vec![(0, spec)],
        };
        self.log_edit(st, &edit)?;
        st.version = Arc::new(st.version.with_edit(&[], vec![(0, meta)])?);
        let old = st.wal_number;
        st.wal = WalWriter::new(wal_file);
        st.wal_number = new_wal;
        st.mem = MemTable::new();
        let _ = self.env.remove(&wal_path(&self.dir, old));
        Ok(())
    }

    fn max_bytes_for_level(&self, level: usize) -> u64 {
        let mut b = self.opts.level_base_bytes;
        for _ in 1..level {
            b = b.saturating_mul(10);
        }
        b
    }

    fn pick_compaction(&self, st: &mut State) -> Option<(usize, Vec<Arc<FileMeta>>)> {
        let v = st.version.clone();
        if v.levels[0].len() >= self.opts.l0_compaction_trigger {
            return Some((0, v.levels[0].clone()));
        }
        for level in 1..NUM_LEVELS - 1 {
            if v.level_bytes(level) > self.max_bytes_for_level(level) {
                let files = &v.levels[level];
                let ptr = &st.compact_pointer[level];
                let f = files
                    .iter()
                    .find(|f| f.spec.smallest > *ptr)
                    .unwrap_or(&files[0])
                    .clone();
                st.compact_pointer[level] = f.spec.largest.clone();
                return Some((level, vec![f]));
            }
        }
        None
    }

    fn maybe_compact(&self, st: &mut State) -> Result<()> {
        while let Some((level, inputs)) = self.pick_compaction(st) {
            let r = self.compact_files(st, level, inputs);
            self.poison_on_err(st, r)?;
        }
        Ok(())
    }

    /// Сливает `inputs` с пересекающимися файлами следующего уровня в новые файлы следующего уровня.
    fn compact_files(
        &self,
        st: &mut State,
        level: usize,
        inputs: Vec<Arc<FileMeta>>,
    ) -> Result<()> {
        let out_level = level + 1;
        let lo = inputs
            .iter()
            .map(|f| f.spec.smallest.as_slice())
            .min()
            .expect("есть входные файлы")
            .to_vec();
        let hi = inputs
            .iter()
            .map(|f| f.spec.largest.as_slice())
            .max()
            .expect("есть входные файлы")
            .to_vec();
        let version = st.version.clone();
        let overlap: Vec<Arc<FileMeta>> = version.levels[out_level]
            .iter()
            .filter(|f| f.overlaps(&lo, &hi))
            .cloned()
            .collect();
        // диапазон слияния шире входных файлов: в него входят и целые файлы следующего уровня
        let all_lo = inputs
            .iter()
            .chain(overlap.iter())
            .map(|f| f.spec.smallest.as_slice())
            .min()
            .expect("есть входные файлы");
        let all_hi = inputs
            .iter()
            .chain(overlap.iter())
            .map(|f| f.spec.largest.as_slice())
            .max()
            .expect("есть входные файлы");
        let bottommost = (out_level + 1..NUM_LEVELS).all(|l| {
            version.levels[l]
                .iter()
                .all(|f| !f.overlaps(all_lo, all_hi))
        });
        let snaps: Vec<u64> = st.snapshots.keys().copied().collect();

        let mut sources: Vec<TableIter<Arc<Table>>> = Vec::new();
        for f in inputs.iter().chain(overlap.iter()) {
            let mut it = TableIter::new(f.table.clone());
            it.seek_to_first()?;
            sources.push(it);
        }

        let mut outputs: Vec<FileSpec> = Vec::new();
        let mut current: Option<(u64, TableBuilder)> = None;
        while let Some(first) = min_source(&sources) {
            let key = sources[first].peek().expect("источник не пуст").key.clone();
            let mut group: Vec<Entry> = Vec::new();
            while let Some(i) = min_source(&sources) {
                if sources[i].peek().expect("источник не пуст").key != key {
                    break;
                }
                group.push(sources[i].peek().expect("источник не пуст").clone());
                sources[i].advance()?;
            }
            let mut kept = retain_versions(group, &snaps);
            if bottommost {
                while kept.last().is_some_and(|e| e.kind == Kind::Delete) {
                    kept.pop();
                }
            }
            if kept.is_empty() {
                continue;
            }
            if current.is_none() {
                let n = Inner::alloc(st);
                current = Some((
                    n,
                    TableBuilder::new(self.env.create_file(&sst_path(&self.dir, n))?),
                ));
            }
            let (_, b) = current.as_mut().expect("построитель создан");
            for e in &kept {
                b.add(&e.key, e.seq, e.kind, &e.value)?;
            }
            if b.size_estimate() >= self.opts.target_file_size {
                let (n, b) = current.take().expect("построитель создан");
                outputs.push(finish_spec(n, b)?);
            }
        }
        if let Some((n, b)) = current.take() {
            outputs.push(finish_spec(n, b)?);
        }

        self.env.sync_dir(&self.dir)?;
        let mut added = Vec::new();
        for spec in &outputs {
            added.push((out_level, self.open_meta(spec.clone())?));
        }
        let mut deleted: Vec<(usize, u64)> =
            inputs.iter().map(|f| (level, f.spec.number)).collect();
        deleted.extend(overlap.iter().map(|f| (out_level, f.spec.number)));
        let edit = VersionEdit {
            log_number: st.wal_number,
            next_file: st.next_file,
            last_seq: st.last_seq,
            deleted: deleted.clone(),
            added: outputs.iter().map(|s| (out_level, s.clone())).collect(),
        };
        self.log_edit(st, &edit)?;
        let numbers: Vec<u64> = deleted.iter().map(|(_, n)| *n).collect();
        st.version = Arc::new(st.version.with_edit(&numbers, added)?);
        for f in inputs.iter().chain(overlap.iter()) {
            f.table
                .mark_obsolete(self.env.clone(), sst_path(&self.dir, f.spec.number));
        }
        Ok(())
    }
}

fn open_meta(env: &dyn Env, dir: &Path, spec: FileSpec) -> Result<Arc<FileMeta>> {
    let table = Table::open(env, &sst_path(dir, spec.number))?;
    if table.entries != spec.entries {
        return corruption(format!(
            "в файле {} записей {}, а манифест ждёт {}",
            spec.number, table.entries, spec.entries
        ));
    }
    Ok(Arc::new(FileMeta {
        spec,
        table: Arc::new(table),
    }))
}

fn finish_spec(number: u64, b: TableBuilder) -> Result<FileSpec> {
    let info = b.finish(true)?;
    Ok(FileSpec {
        number,
        size: info.size,
        entries: info.entries,
        smallest: info.smallest,
        largest: info.largest,
    })
}

/// Источник с наименьшей текущей записью во внутреннем порядке.
fn min_source(sources: &[TableIter<Arc<Table>>]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, s) in sources.iter().enumerate() {
        if let Some(e) = s.peek() {
            let better = match best {
                None => true,
                Some(b) => {
                    let o = sources[b].peek().expect("лучший источник не пуст");
                    cmp_internal(&e.key, e.seq, &o.key, o.seq).is_lt()
                }
            };
            if better {
                best = Some(i);
            }
        }
    }
    best
}

/// Из версий одного ключа (новые первыми) оставляет нужные: самую новую и по одной для каждого снимка.
fn retain_versions(group: Vec<Entry>, snaps: &[u64]) -> Vec<Entry> {
    let mut kept: Vec<Entry> = Vec::new();
    let mut prev_seq = u64::MAX;
    for e in group {
        let needed = kept.is_empty() || snaps.iter().any(|&s| e.seq <= s && s < prev_seq);
        if needed {
            prev_seq = e.seq;
            kept.push(e);
        }
    }
    kept
}

fn write_mem_table(env: &dyn Env, dir: &Path, number: u64, mem: &MemTable) -> Result<FileSpec> {
    let mut b = TableBuilder::new(env.create_file(&sst_path(dir, number))?);
    for (k, s, kind, v) in mem.iter() {
        b.add(k, s, kind, v)?;
    }
    finish_spec(number, b)
}

impl Db {
    /// Открывает (или создаёт) базу в каталоге на диске.
    pub fn open(dir: impl AsRef<Path>, opts: Options) -> Result<Db> {
        Db::open_with_env(Arc::new(RealEnv), dir.as_ref(), opts)
    }

    pub fn open_with_env(env: Arc<dyn Env>, dir: &Path, opts: Options) -> Result<Db> {
        env.create_dir_all(dir)?;
        let lock = env.lock(&dir.join("LOCK")).map_err(|e| {
            if e.kind() == io::ErrorKind::WouldBlock {
                Error::Locked(e.to_string())
            } else {
                Error::Io(e)
            }
        })?;

        let mut ms = ManifestState::default();
        let current = dir.join("CURRENT");
        if env.exists(&current) {
            let raw = env.read_file(&current)?;
            let name =
                String::from_utf8(raw).map_err(|_| Error::Corruption("CURRENT не текст".into()))?;
            let name = name.trim();
            if !name.starts_with("MANIFEST-") || name.contains(['/', '\\']) {
                return corruption(format!("CURRENT указывает на странное имя «{name}»"));
            }
            let data = env.read_file(&dir.join(name))?;
            for rec in wal::replay(&data)?.records {
                ms.apply(&VersionEdit::decode(rec)?)?;
            }
        }

        let names = env.list(dir)?;
        let max_on_disk = names
            .iter()
            .filter_map(|n| {
                parse_number(n, ".wal")
                    .or_else(|| parse_number(n, ".sst"))
                    .or_else(|| n.strip_prefix("MANIFEST-").and_then(|s| s.parse().ok()))
            })
            .max()
            .unwrap_or(0);
        let mut next_file = ms.next_file.max(max_on_disk + 1).max(2);

        let mut files: Vec<(usize, Arc<FileMeta>)> = Vec::new();
        for (level, spec) in ms.files.values() {
            files.push((*level, open_meta(&*env, dir, spec.clone())?));
        }

        // восстановление: проигрываем журналы, которые ещё не попали в файлы
        let mut wals: Vec<u64> = names
            .iter()
            .filter_map(|n| parse_number(n, ".wal"))
            .filter(|&n| n >= ms.log_number)
            .collect();
        wals.sort_unstable();
        let mut mem = MemTable::new();
        let mut last_seq = ms.last_seq;
        for n in wals {
            let data = env.read_file(&wal_path(dir, n))?;
            for rec in wal::replay(&data)?.records {
                let (seq, batch) = WriteBatch::decode(rec)?;
                let count = batch.ops.len() as u64;
                for (i, op) in batch.ops.into_iter().enumerate() {
                    mem.insert(op.key, seq + i as u64, op.kind, op.value);
                }
                last_seq = last_seq.max(seq.saturating_add(count.saturating_sub(1)));
            }
        }
        if !mem.is_empty() {
            let n = next_file;
            next_file += 1;
            let spec = write_mem_table(&*env, dir, n, &mem)?;
            files.push((0, open_meta(&*env, dir, spec.clone())?));
            ms.files.insert(n, (0, spec));
        }

        // новый журнал и новый манифест; CURRENT переключается последним
        let wal_number = next_file;
        let manifest_number = next_file + 1;
        next_file += 2;
        let wal_file = env.create_file(&wal_path(dir, wal_number))?;
        ms.log_number = wal_number;
        ms.next_file = next_file;
        ms.last_seq = last_seq;
        let mut manifest =
            WalWriter::new(env.create_file(&dir.join(manifest_name(manifest_number)))?);
        manifest.add_record(&ms.to_edit().encode(), true)?;
        let tmp = dir.join("CURRENT.tmp");
        let mut t = env.create_file(&tmp)?;
        t.append(format!("{}\n", manifest_name(manifest_number)).as_bytes())?;
        t.sync()?;
        drop(t);
        env.sync_dir(dir)?;
        env.rename(&tmp, &current)?;
        env.sync_dir(dir)?;

        // уборка: всё, что не нужно новой версии
        for n in &names {
            let keep = parse_number(n, ".sst").is_some_and(|x| ms.files.contains_key(&x));
            let junk = parse_number(n, ".sst").is_some()
                || parse_number(n, ".wal").is_some()
                || n.starts_with("MANIFEST-")
                || n == "CURRENT.tmp";
            if junk && !keep {
                let _ = env.remove(&dir.join(n));
            }
        }

        let version = Arc::new(Version::build(files)?);
        let state = State {
            mem: MemTable::new(),
            wal: WalWriter::new(wal_file),
            wal_number,
            manifest,
            version,
            next_file,
            last_seq,
            snapshots: BTreeMap::new(),
            compact_pointer: vec![Vec::new(); NUM_LEVELS],
            poisoned: None,
            _lock: lock,
        };
        let db = Db {
            inner: Arc::new(Inner {
                env,
                dir: dir.to_path_buf(),
                opts,
                state: Mutex::new(state),
            }),
        };
        {
            let mut st = db.inner.lock();
            db.inner.maybe_compact(&mut st)?;
        }
        Ok(db)
    }

    pub fn put(&self, key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Result<()> {
        let mut b = WriteBatch::new();
        b.put(key, value);
        self.write(&b)
    }

    pub fn delete(&self, key: impl Into<Vec<u8>>) -> Result<()> {
        let mut b = WriteBatch::new();
        b.delete(key);
        self.write(&b)
    }

    /// Применяет пакет атомарно; fsync по настройке `Options::sync`.
    pub fn write(&self, batch: &WriteBatch) -> Result<()> {
        self.write_opt(batch, self.inner.opts.sync)
    }

    /// Применяет пакет атомарно. Без `sync` запись может пропасть при потере питания (но не повредит базу).
    ///
    /// Ошибка означает, что исход неизвестен: запись могла и сохраниться. После ошибки записи база закрывается для работы.
    pub fn write_opt(&self, batch: &WriteBatch, sync: bool) -> Result<()> {
        batch.validate()?;
        if batch.is_empty() {
            return Ok(());
        }
        let mut st = self.inner.lock();
        Inner::check(&st)?;
        let first = st.last_seq + 1;
        let rec = batch.encode(first);
        if rec.len() > wal::MAX_RECORD {
            return Err(Error::InvalidArgument("пакет слишком большой".into()));
        }
        if let Err(e) = st.wal.add_record(&rec, sync) {
            st.poisoned = Some(e.to_string());
            return Err(e.into());
        }
        for (i, op) in batch.ops.iter().enumerate() {
            st.mem
                .insert(op.key.clone(), first + i as u64, op.kind, op.value.clone());
        }
        st.last_seq += batch.len() as u64;
        if st.mem.approximate_bytes() >= self.inner.opts.write_buffer_size {
            self.inner.flush_locked(&mut st)?;
            self.inner.maybe_compact(&mut st)?;
        }
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_seq(key, None)
    }

    pub fn get_at(&self, snap: &Snapshot, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_seq(key, Some(snap.seq))
    }

    fn get_seq(&self, key: &[u8], seq: Option<u64>) -> Result<Option<Vec<u8>>> {
        let (mem_hit, version, seq) = {
            let st = self.inner.lock();
            Inner::check(&st)?;
            let seq = seq.unwrap_or(st.last_seq);
            if let Some((_, kind, v)) = st.mem.get(key, seq) {
                (
                    Some(if kind == Kind::Put {
                        Some(v.to_vec())
                    } else {
                        None
                    }),
                    None,
                    seq,
                )
            } else {
                (None, Some(st.version.clone()), seq)
            }
        };
        if let Some(r) = mem_hit {
            return Ok(r);
        }
        let version = version.expect("версия взята, если в memtable нет записи");
        let found = |e: Entry| {
            if e.kind == Kind::Put {
                Some(e.value)
            } else {
                None
            }
        };
        for f in version.levels[0].iter().rev() {
            if f.spec.smallest.as_slice() <= key
                && key <= f.spec.largest.as_slice()
                && let Some(e) = f.table.get(key, seq)?
            {
                return Ok(found(e));
            }
        }
        for level in 1..NUM_LEVELS {
            if let Some(f) = version.find_file(level, key)
                && let Some(e) = f.table.get(key, seq)?
            {
                return Ok(found(e));
            }
        }
        Ok(None)
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut st = self.inner.lock();
        let seq = st.last_seq;
        *st.snapshots.entry(seq).or_insert(0) += 1;
        Snapshot {
            inner: self.inner.clone(),
            seq,
        }
    }

    /// Просмотр ключей из `[start, end)` по порядку на текущий момент.
    pub fn scan(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<DbIter> {
        self.scan_seq(start, end, None)
    }

    pub fn scan_at(
        &self,
        snap: &Snapshot,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<DbIter> {
        self.scan_seq(start, end, Some(snap.seq))
    }

    fn scan_seq(
        &self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
        seq: Option<u64>,
    ) -> Result<DbIter> {
        let (mem, version, seq) = {
            let st = self.inner.lock();
            Inner::check(&st)?;
            (
                st.mem.entries_in(start, end),
                st.version.clone(),
                seq.unwrap_or(st.last_seq),
            )
        };
        let mut sources = vec![Source::Mem(mem, 0)];
        for level in &version.levels {
            for f in level {
                if start.is_some_and(|s| f.spec.largest.as_slice() < s)
                    || end.is_some_and(|e| f.spec.smallest.as_slice() >= e)
                {
                    continue;
                }
                let mut it = TableIter::new(f.table.clone());
                match start {
                    Some(s) => it.seek(s, u64::MAX)?,
                    None => it.seek_to_first()?,
                }
                sources.push(Source::Table(it));
            }
        }
        Ok(DbIter {
            sources,
            end: end.map(<[u8]>::to_vec),
            seq,
            last_key: None,
            done: false,
            _pin: version,
        })
    }

    /// Сбрасывает memtable в файл.
    pub fn flush(&self) -> Result<()> {
        let mut st = self.inner.lock();
        Inner::check(&st)?;
        self.inner.flush_locked(&mut st)
    }

    /// Сбрасывает memtable и сливает все уровни вниз: убирает устаревшие версии и надгробия.
    pub fn compact(&self) -> Result<()> {
        let mut st = self.inner.lock();
        Inner::check(&st)?;
        self.inner.flush_locked(&mut st)?;
        let deepest = st.version.deepest_nonempty().clamp(1, NUM_LEVELS - 1);
        for level in 0..deepest {
            let files = st.version.levels[level].clone();
            if !files.is_empty() {
                let r = self.inner.compact_files(&mut st, level, files);
                self.inner.poison_on_err(&mut st, r)?;
            }
        }
        Ok(())
    }

    pub fn stats(&self) -> Stats {
        let st = self.inner.lock();
        Stats {
            files_per_level: st.version.levels.iter().map(Vec::len).collect(),
            bytes_per_level: (0..NUM_LEVELS).map(|l| st.version.level_bytes(l)).collect(),
            memtable_bytes: st.mem.approximate_bytes(),
            last_seq: st.last_seq,
            live_snapshots: st.snapshots.values().sum(),
        }
    }
}

enum Source {
    Mem(Vec<Entry>, usize),
    Table(TableIter<Arc<Table>>),
}

impl Source {
    fn peek(&self) -> Option<&Entry> {
        match self {
            Source::Mem(v, i) => v.get(*i),
            Source::Table(t) => t.peek(),
        }
    }
    fn advance(&mut self) -> Result<()> {
        match self {
            Source::Mem(_, i) => {
                *i += 1;
                Ok(())
            }
            Source::Table(t) => t.advance(),
        }
    }
}

/// Итератор по диапазону ключей: выдаёт пары (ключ, значение) по возрастанию ключа.
pub struct DbIter {
    sources: Vec<Source>,
    end: Option<Vec<u8>>,
    seq: u64,
    last_key: Option<Vec<u8>>,
    done: bool,
    _pin: Arc<Version>,
}

impl Iterator for DbIter {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let mut best: Option<usize> = None;
            for (i, s) in self.sources.iter().enumerate() {
                if let Some(e) = s.peek() {
                    let better = match best {
                        None => true,
                        Some(b) => {
                            let o = self.sources[b].peek().expect("лучший источник не пуст");
                            cmp_internal(&e.key, e.seq, &o.key, o.seq).is_lt()
                        }
                    };
                    if better {
                        best = Some(i);
                    }
                }
            }
            let Some(i) = best else {
                self.done = true;
                return None;
            };
            let e = self.sources[i].peek().expect("источник не пуст").clone();
            if self
                .end
                .as_ref()
                .is_some_and(|end| e.key.as_slice() >= end.as_slice())
            {
                self.done = true;
                return None;
            }
            if let Err(err) = self.sources[i].advance() {
                self.done = true;
                return Some(Err(err));
            }
            if e.seq > self.seq || self.last_key.as_deref() == Some(e.key.as_slice()) {
                continue;
            }
            self.last_key = Some(e.key.clone());
            if e.kind == Kind::Put {
                return Some(Ok((e.key, e.value)));
            }
        }
    }
}
