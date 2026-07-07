//! Единый журнал приложения (XR-042).
//!
//! Все источники (движок, пробы доверенной сети, смены сети/режима, файловые
//! операции) пишут в один буфер. Журнал живёт отдельно от движка, поэтому
//! перезапуск движка (смена сети, пауза) ленту не обнуляет; с указанной
//! директорией записи дописываются в файл и переживают перезапуск приложения.
//!
//! Формат строки: `YYYY-MM-DD HH:MM:SS LEVEL [source] message` (время UTC,
//! уровень выровнен по ширине 5, как раньше в `Stats`). Подряд идущие
//! одинаковые записи сворачиваются в `... (×N)`.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Имя текущего файла журнала; ротированные получают суффиксы `.1`, `.2`, ...
const FILE_NAME: &str = "journal.log";

/// Сколько последних строк держим в памяти для вкладки Log.
const MAX_TAIL: usize = 400;

/// Format current wall-clock time as YYYY-MM-DD HH:MM:SS UTC.
pub(crate) fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Days since epoch -> date (simplified, no leap second handling).
    let days = (secs / 86400) as i64;
    let time = secs % 86400;
    let h = time / 3600;
    let m = (time % 3600) / 60;
    let s = time % 60;

    // Civil date from days since 1970-01-01 (Rata Die algorithm).
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };

    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}", y, mo, d, h, m, s)
}

/// Parse "... (×N)" suffix. Returns (core, count). If no suffix, count is 1.
/// Note: `×` is U+00D7 (2 bytes in UTF-8); byte-slicing is safe here because
/// we only split at ASCII boundaries (the leading space and trailing `)`).
fn split_count_suffix(s: &str) -> (&str, u64) {
    if !s.ends_with(')') { return (s, 1); }
    let Some(open) = s.rfind(" (×") else { return (s, 1); };
    // Offsets: " " = 1 byte, "(" = 1, "×" = 2 -> 4 bytes total.
    let num_start = open + 4;
    let num_end = s.len() - 1;
    if num_start >= num_end { return (s, 1); }
    let num_str = &s[num_start..num_end];
    match num_str.parse::<u64>() {
        Ok(n) => (&s[..open], n),
        Err(_) => (s, 1),
    }
}

struct JournalInner {
    /// Хвост для быстрого чтения из UI (каждую секунду по тику опроса).
    tail: VecDeque<String>,
    /// Персист: `None` в memory-only режиме (тесты, движок без Android-обвязки).
    dir: Option<PathBuf>,
    file: Option<File>,
    file_len: u64,
    /// Смещение начала последней строки в текущем файле: коалесация
    /// переписывает её на месте (`set_len` + повторная запись).
    last_line_offset: Option<u64>,
    /// Порог ротации текущего файла.
    max_file_bytes: u64,
    /// Сколько файлов держим всего, включая текущий (минимум 1).
    max_files: u32,
}

/// Потокобезопасный журнал; `Clone` разделяет один буфер.
#[derive(Clone)]
pub struct Journal {
    inner: Arc<Mutex<JournalInner>>,
}

impl Journal {
    /// Журнал без персиста: только хвост в памяти.
    pub fn memory() -> Self {
        Self {
            inner: Arc::new(Mutex::new(JournalInner {
                tail: VecDeque::new(),
                dir: None,
                file: None,
                file_len: 0,
                last_line_offset: None,
                max_file_bytes: u64::MAX,
                max_files: 1,
            })),
        }
    }

    /// Файловый журнал в `dir` (создаётся при необходимости); хвост
    /// подгружается с диска, так что лента видна сразу после запуска
    /// приложения. Любая ошибка ввода-вывода деградирует в memory-only,
    /// журнал никогда не валит вызывающего.
    pub fn open(dir: PathBuf, max_file_bytes: u64, max_files: u32) -> Self {
        let j = Self::memory();
        {
            let mut inner = j.inner.lock().unwrap();
            inner.max_file_bytes = max_file_bytes.max(4 * 1024);
            inner.max_files = max_files.max(1);
            if std::fs::create_dir_all(&dir).is_ok() {
                inner.dir = Some(dir);
                inner.load_from_disk();
            }
        }
        j
    }

    /// Обновить параметры ротации на лету (настройки в приложении).
    pub fn set_rotation(&self, max_file_bytes: u64, max_files: u32) {
        let mut inner = self.inner.lock().unwrap();
        let old_max_files = inner.max_files;
        inner.max_file_bytes = max_file_bytes.max(4 * 1024);
        inner.max_files = max_files.max(1);
        // Кол-во файлов уменьшили: хвосты за новым пределом иначе останутся
        // на диске навсегда (ротация их не сдвинет, dump не прочитает).
        if let Some(dir) = inner.dir.as_ref() {
            for i in inner.max_files.max(1)..old_max_files.max(1) {
                let _ = std::fs::remove_file(dir.join(format!("{}.{}", FILE_NAME, i)));
            }
        }
        inner.rotate_if_needed();
    }

    /// Дописать запись. `level` ожидается из {"INFO","WARN","ERROR"},
    /// `source` это короткий тег источника ("vpn", "net", "probe", "files").
    pub fn append(&self, level: &str, source: &str, msg: &str) {
        // Ширина уровня 5, чтобы "ERROR" (5) и "WARN"/"INFO" (4) выровнялись.
        let entry = format!("{} {:>5} [{}] {}", timestamp(), level, source, msg);
        let mut inner = self.inner.lock().unwrap();

        // Сворачивание дубликатов. Когда подряд приходят одинаковые записи
        // (напр. burst `geller-pa.googleapis.com` 50 раз за секунду), читать
        // лог становится невозможно, а файл пухнет. Сравниваем с последней
        // записью по core (без суффикса " (×N)"): совпала, значит переписываем
        // last на "core (×N+1)" и в памяти, и в файле. Timestamp уже внутри
        // core, так что свёртка естественно ограничена одной секундой.
        if let Some(last) = inner.tail.back() {
            let (last_core, last_count) = split_count_suffix(last);
            let (new_core, _) = split_count_suffix(&entry);
            if last_core == new_core {
                let merged = format!("{} (×{})", last_core, last_count + 1);
                *inner.tail.back_mut().unwrap() = merged.clone();
                inner.rewrite_last_line(&merged);
                return;
            }
        }

        inner.push_tail(entry.clone());
        inner.write_line(&entry);
    }

    /// Хвост журнала для вкладки Log (последние [`MAX_TAIL`] строк).
    pub fn tail(&self) -> Vec<String> {
        self.inner.lock().unwrap().tail.iter().cloned().collect()
    }

    /// Полное содержимое журнала с диска, от старых записей к новым
    /// (экспорт/шаринг лога). В memory-only режиме отдаёт хвост.
    pub fn dump(&self) -> String {
        let inner = self.inner.lock().unwrap();
        let Some(dir) = inner.dir.as_ref() else {
            let mut s = inner.tail.iter().cloned().collect::<Vec<_>>().join("\n");
            if !s.is_empty() { s.push('\n'); }
            return s;
        };
        let mut out = String::new();
        for i in (1..inner.max_files).rev() {
            let path = dir.join(format!("{}.{}", FILE_NAME, i));
            if let Ok(mut f) = File::open(&path) {
                let _ = f.read_to_string(&mut out);
            }
        }
        if let Ok(mut f) = File::open(dir.join(FILE_NAME)) {
            let _ = f.read_to_string(&mut out);
        }
        out
    }

    /// Очистить журнал целиком: хвост и все файлы.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.tail.clear();
        inner.last_line_offset = None;
        inner.file_len = 0;
        if let Some(dir) = inner.dir.clone() {
            for i in 1..inner.max_files.max(2) {
                let _ = std::fs::remove_file(dir.join(format!("{}.{}", FILE_NAME, i)));
            }
            if let Some(f) = inner.file.as_mut() {
                let _ = f.set_len(0);
                let _ = f.seek(SeekFrom::Start(0));
            } else {
                let _ = std::fs::remove_file(dir.join(FILE_NAME));
            }
        }
    }
}

impl JournalInner {
    /// Открыть текущий файл и восстановить хвост с диска. Вызывается под
    /// локом из `Journal::open`.
    fn load_from_disk(&mut self) {
        let Some(dir) = self.dir.as_ref() else { return };
        let path = dir.join(FILE_NAME);
        // Не append-режим: коалесация переписывает последнюю строку через
        // seek, а append-режим в POSIX всегда пишет в конец.
        let Ok(mut file) = OpenOptions::new().read(true).write(true).create(true).open(&path)
        else { return };

        let mut content = String::new();
        if file.read_to_string(&mut content).is_err() {
            // Файл побился (не-UTF8): начинаем с чистого листа.
            let _ = file.set_len(0);
            content.clear();
        }
        // Оборванная крэшем последняя строка без \n: закрываем её, чтобы
        // следующая запись не приклеилась.
        if !content.is_empty() && !content.ends_with('\n') {
            let _ = file.write_all(b"\n");
            content.push('\n');
        }
        self.file_len = content.len() as u64;

        // Хвост: последние MAX_TAIL строк текущего файла, при нехватке
        // добираем из свежайшего ротированного (глубже смысла нет, это
        // только стартовая картинка для UI).
        let mut merged: Vec<String> = Vec::new();
        if content.lines().count() < MAX_TAIL {
            if let Ok(prev) = std::fs::read_to_string(dir.join(format!("{}.1", FILE_NAME))) {
                merged.extend(prev.lines().map(str::to_owned));
            }
        }
        merged.extend(content.lines().map(str::to_owned));
        let skip = merged.len().saturating_sub(MAX_TAIL);
        self.tail = merged.into_iter().skip(skip).collect();

        // Смещение последней строки для коалесации после рестарта.
        self.last_line_offset = content
            .trim_end_matches('\n')
            .rfind('\n')
            .map(|p| (p + 1) as u64)
            .or(if content.is_empty() { None } else { Some(0) });

        self.file = Some(file);
        self.rotate_if_needed();
    }

    fn push_tail(&mut self, entry: String) {
        if self.tail.len() >= MAX_TAIL {
            // Трёхуровневый приоритетный drain, как раньше в Stats:
            //   1. Сначала 50 самых старых INFO скидываем.
            //   2. Если всё ещё переполнено, 50 самых старых WARN.
            //   3. В крайнем случае 50 любых самых старых.
            // ERROR никогда не вытесняются INFO/WARN-шумом, поэтому бадж и
            // заголовок вкладки Log всегда честно показывают реальные отказы.
            // На диске при этом остаётся всё (до предела ротации).
            let mut to_drop = 50usize;
            self.tail.retain(|e| {
                if to_drop == 0 { return true; }
                if e.contains(" WARN ") || e.contains(" ERROR ") { return true; }
                to_drop -= 1;
                false
            });
            if self.tail.len() >= MAX_TAIL {
                let mut to_drop_warn = 50usize;
                self.tail.retain(|e| {
                    if to_drop_warn == 0 { return true; }
                    if e.contains(" ERROR ") { return true; }
                    to_drop_warn -= 1;
                    false
                });
            }
            if self.tail.len() >= MAX_TAIL {
                self.tail.drain(0..50);
            }
        }
        self.tail.push_back(entry);
    }

    fn write_line(&mut self, entry: &str) {
        if self.file.is_none() { return; }
        self.rotate_if_needed();
        let Some(f) = self.file.as_mut() else { return };
        if f.seek(SeekFrom::Start(self.file_len)).is_err() { return; }
        let line = format!("{}\n", entry);
        if f.write_all(line.as_bytes()).is_ok() {
            self.last_line_offset = Some(self.file_len);
            self.file_len += line.len() as u64;
        }
    }

    /// Переписать последнюю строку файла (коалесация "(×N)").
    fn rewrite_last_line(&mut self, entry: &str) {
        let (Some(f), Some(off)) = (self.file.as_mut(), self.last_line_offset) else { return };
        if f.set_len(off).is_err() || f.seek(SeekFrom::Start(off)).is_err() { return; }
        let line = format!("{}\n", entry);
        if f.write_all(line.as_bytes()).is_ok() {
            self.file_len = off + line.len() as u64;
        } else {
            self.file_len = off;
        }
    }

    /// Ротация по размеру: текущий файл уходит в `.1`, старые сдвигаются,
    /// самый старый удаляется. Максимум `max_files` файлов, включая текущий.
    fn rotate_if_needed(&mut self) {
        if self.file.is_none() || self.file_len < self.max_file_bytes { return; }
        let Some(dir) = self.dir.clone() else { return };
        self.file = None;

        if self.max_files == 1 {
            // Без хвостовых файлов: просто начинаем текущий заново.
            let _ = std::fs::remove_file(dir.join(FILE_NAME));
        } else {
            let _ = std::fs::remove_file(dir.join(format!("{}.{}", FILE_NAME, self.max_files - 1)));
            for i in (1..self.max_files - 1).rev() {
                let _ = std::fs::rename(
                    dir.join(format!("{}.{}", FILE_NAME, i)),
                    dir.join(format!("{}.{}", FILE_NAME, i + 1)),
                );
            }
            let _ = std::fs::rename(dir.join(FILE_NAME), dir.join(format!("{}.1", FILE_NAME)));
        }

        self.file_len = 0;
        self.last_line_offset = None;
        self.file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(dir.join(FILE_NAME))
            .ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_journal(max_bytes: u64, max_files: u32) -> (tempfile::TempDir, Journal) {
        let dir = tempfile::tempdir().unwrap();
        let j = Journal::open(dir.path().to_path_buf(), max_bytes, max_files);
        (dir, j)
    }

    #[test]
    fn coalesce_duplicate_entries() {
        let j = Journal::memory();
        j.append("INFO", "vpn", "mux relay for Domain(\"x.example\", 443)");
        j.append("INFO", "vpn", "mux relay for Domain(\"x.example\", 443)");
        j.append("INFO", "vpn", "mux relay for Domain(\"x.example\", 443)");

        let entries = j.tail();
        assert_eq!(entries.len(), 1, "entries: {:?}", entries);
        assert!(entries[0].ends_with(" (\u{00D7}3)"), "unexpected: {}", entries[0]);
    }

    #[test]
    fn different_messages_not_coalesced() {
        let j = Journal::memory();
        j.append("INFO", "vpn", "mux relay for X");
        j.append("INFO", "vpn", "mux relay for Y");
        j.append("INFO", "vpn", "mux relay for X");

        let entries = j.tail();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().all(|e| !e.contains("(\u{00D7}")));
    }

    #[test]
    fn sources_not_coalesced_across() {
        let j = Journal::memory();
        j.append("INFO", "vpn", "same text");
        j.append("INFO", "net", "same text");

        // Разные источники это разные записи, даже с одинаковым текстом.
        assert_eq!(j.tail().len(), 2);
    }

    #[test]
    fn drain_prefers_info_over_warn_and_error() {
        let j = Journal::memory();
        j.append("ERROR", "vpn", "mux open fail: initial1");
        j.append("ERROR", "vpn", "mux open fail: initial2");
        j.append("ERROR", "vpn", "mux open fail: initial3");
        j.append("WARN", "vpn", "fake IP without domain");
        j.append("WARN", "vpn", "private IP blocked");
        for i in 0..500 {
            j.append("INFO", "vpn", &format!("mux relay for target-{}", i));
        }

        let entries = j.tail();
        let error_count = entries.iter().filter(|e| e.contains(" ERROR ")).count();
        let warn_count = entries.iter().filter(|e| e.contains(" WARN ")).count();
        assert_eq!(error_count, 3, "все 3 ERROR должны остаться");
        assert_eq!(warn_count, 2, "все 2 WARN должны остаться");
        assert!(entries.len() <= MAX_TAIL);
    }

    #[test]
    fn drain_falls_back_when_only_errors() {
        let j = Journal::memory();
        for i in 0..500 {
            j.append("ERROR", "vpn", &format!("fatal-{}", i));
        }
        let entries = j.tail();
        assert!(entries.len() <= MAX_TAIL);
        assert!(entries.last().unwrap().contains("fatal-499"));
        assert!(!entries.iter().any(|e| e.ends_with("fatal-0")));
    }

    #[test]
    fn split_count_suffix_parses() {
        assert_eq!(split_count_suffix("hello"), ("hello", 1));
        assert_eq!(split_count_suffix("hello (\u{00D7}5)"), ("hello", 5));
        assert_eq!(split_count_suffix("hello (\u{00D7}42)"), ("hello", 42));
        assert_eq!(split_count_suffix("hello (world)"), ("hello (world)", 1));
        assert_eq!(split_count_suffix("hello (\u{00D7}x)"), ("hello (\u{00D7}x)", 1));
    }

    #[test]
    fn entry_format_has_level_and_source() {
        let j = Journal::memory();
        j.append("WARN", "files", "скачивание сорвалось");
        let e = &j.tail()[0];
        // "2026-01-01 00:00:00  WARN [files] ..." (уровень выровнен до 5).
        assert!(e.contains("  WARN [files] скачивание сорвалось"), "unexpected: {}", e);
    }

    // ── персист ──────────────────────────────────────────────────────

    #[test]
    fn survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        {
            let j = Journal::open(dir.path().to_path_buf(), 1 << 20, 3);
            j.append("INFO", "vpn", "первая запись");
            j.append("ERROR", "vpn", "вторая запись");
        }
        // Регрессия XR-042: лента должна пережить перезапуск (раньше
        // recentErrors жил внутри движка и обнулялся при смене сети).
        let j = Journal::open(dir.path().to_path_buf(), 1 << 20, 3);
        let entries = j.tail();
        assert_eq!(entries.len(), 2, "entries: {:?}", entries);
        assert!(entries[0].contains("первая запись"));
        assert!(entries[1].contains("вторая запись"));
    }

    #[test]
    fn coalesce_persists_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        {
            let j = Journal::open(dir.path().to_path_buf(), 1 << 20, 3);
            j.append("INFO", "vpn", "same");
            j.append("INFO", "vpn", "same");
            j.append("INFO", "vpn", "same");
        }
        let content = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        // В файле одна строка со свёрнутым счётчиком, а не три.
        assert_eq!(content.lines().count(), 1, "content: {}", content);
        assert!(content.trim_end().ends_with("(\u{00D7}3)"));

        // И после перезапуска свёртка продолжается с той же строки.
        let j = Journal::open(dir.path().to_path_buf(), 1 << 20, 3);
        j.append("INFO", "vpn", "same");
        // Timestamp в core может отличаться (прошла секунда), тогда будет
        // вторая строка; главное, что файл не растёт по строке на дубликат.
        assert!(j.tail().len() <= 2);
    }

    #[test]
    fn rotation_by_size_keeps_max_files() {
        let (dir, j) = temp_journal(4 * 1024, 2);
        // ~100 байт на строку, 4КиБ порог: сотни записей дают несколько ротаций.
        for i in 0..500 {
            j.append("INFO", "vpn", &format!("запись номер {} с балластом для объёма", i));
        }
        let cur = dir.path().join(FILE_NAME);
        let old1 = dir.path().join(format!("{}.1", FILE_NAME));
        let old2 = dir.path().join(format!("{}.2", FILE_NAME));
        assert!(cur.exists());
        assert!(old1.exists(), "должен существовать один ротированный файл");
        assert!(!old2.exists(), "max_files=2 не оставляет второго хвоста");

        // Dump склеивает старое и новое в хронологическом порядке: начинается
        // с первой строки ротированного файла, заканчивается свежайшей записью.
        let dump = j.dump();
        let first_old = std::fs::read_to_string(&old1).unwrap();
        assert!(dump.starts_with(first_old.lines().next().unwrap()));
        assert!(dump.trim_end().ends_with("запись номер 499 с балластом для объёма"));
    }

    #[test]
    fn shrinking_max_files_removes_orphan_tails() {
        let (dir, j) = temp_journal(4 * 1024, 4);
        for i in 0..600 {
            j.append("INFO", "vpn", &format!("запись {} с балластом для ротации файлов", i));
        }
        assert!(dir.path().join(format!("{}.2", FILE_NAME)).exists());

        j.set_rotation(4 * 1024, 2);
        assert!(!dir.path().join(format!("{}.2", FILE_NAME)).exists());
        assert!(!dir.path().join(format!("{}.3", FILE_NAME)).exists());
        assert!(dir.path().join(format!("{}.1", FILE_NAME)).exists());
    }

    #[test]
    fn clear_removes_everything() {
        let (dir, j) = temp_journal(4 * 1024, 3);
        for i in 0..200 {
            j.append("INFO", "vpn", &format!("запись {} с балластом для ротации файлов", i));
        }
        j.clear();
        assert!(j.tail().is_empty());
        assert!(j.dump().is_empty());
        assert!(!dir.path().join(format!("{}.1", FILE_NAME)).exists());

        // После очистки журнал продолжает писать.
        j.append("INFO", "vpn", "после очистки");
        assert_eq!(j.tail().len(), 1);
        assert!(j.dump().contains("после очистки"));
    }

    #[test]
    fn shared_clone_sees_same_buffer() {
        // Два клона (глобальный журнал в JNI и Stats внутри движка) пишут в
        // один буфер; перезапуск движка со свежим клоном ленту не трогает.
        let j = Journal::memory();
        let engine_side = j.clone();
        engine_side.append("ERROR", "vpn", "до перезапуска");
        drop(engine_side);
        let engine_side2 = j.clone();
        engine_side2.append("INFO", "vpn", "после перезапуска");
        assert_eq!(j.tail().len(), 2);
    }

    #[test]
    fn truncated_last_line_is_closed_on_reopen() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "оборванная строка без переноса").unwrap();
        let j = Journal::open(dir.path().to_path_buf(), 1 << 20, 3);
        j.append("INFO", "vpn", "новая запись");
        let entries = j.tail();
        assert_eq!(entries.len(), 2, "entries: {:?}", entries);
        assert!(entries[1].contains("новая запись"));
    }
}
