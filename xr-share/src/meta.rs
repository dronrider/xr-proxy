//! Origin of the files in a share (XR-255): where a file was imported from,
//! which channel published it, when, and under what title. The manifest carries
//! it to the consumer (see [`xr_proto::share::FileMeta`]); this module is the
//! agent's side of it.
//!
//! **Storage.** One index per share, the JSON file [`INDEX_NAME`] in the share
//! root, keyed by the share-relative path of the file. The reserved `.xr-`
//! namespace already hides it from the listing, from `safepath` and from the
//! import sweep, so it is invisible to consumers and unreachable by a route.
//!
//! The alternative was a sidecar next to every file. An index wins on the read
//! path that matters: a listing is built on every browse and must stay instant
//! even on a cold cache (XR-039), and one small JSON read beats an open per
//! file. It also keeps the owner's own directories clean of service files.
//!
//! **The key is the path, not the SHA-256.** The listing deliberately never
//! hashes, so a freshly imported file has no hash until the warmer gets to it
//! and a hash-keyed row would be invisible exactly when it is most interesting.
//! Beyond that, a hash merges two identical files into one origin and loses the
//! origin entirely when a file is re-encoded, while the path is already the
//! identity of both the manifest row and the sync diff. The price is that a
//! rename outside the agent orphans a row; the agent has no rename route, its
//! own `PUT`/`DELETE` drop the row, and [`backfill`] sweeps orphans at startup.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use xr_proto::share::FileMeta;

/// The per-share index in the share root, inside the reserved `.xr-` namespace.
pub const INDEX_NAME: &str = ".xr-meta.json";

/// Temp name the index is written through before the atomic rename. Also
/// reserved, so a crash mid-write leaves nothing a consumer can see.
const INDEX_TEMP: &str = ".xr-meta.json.new";

/// What a plugin leaves in its job dir for the agent to pick up (LLD-29 п. 2.4):
/// one tab-separated line per output file. Hidden, so it is never published.
pub const PLUGIN_FILE: &str = ".xr-meta.tsv";

/// Serializes the read-modify-write of every index. Writes are rare (an import,
/// an upload, a delete), so one lock for all shares is enough and keeps two
/// concurrent jobs in the same share from dropping each other's rows.
static WRITE_LOCK: Mutex<()> = Mutex::new(());

/// On-disk shape of the index. A `BTreeMap` so the file is stable across writes
/// (readable diffs, reproducible tests).
#[derive(Default, Serialize, Deserialize)]
struct Index {
    files: BTreeMap<String, FileMeta>,
}

/// Read a share's index. A missing file is an empty index; an unreadable or
/// corrupt one is also empty, with a warning, because metadata is a nicety and
/// must never take the listing down with it.
pub fn load(root: &Path) -> BTreeMap<String, FileMeta> {
    let path = root.join(INDEX_NAME);
    let raw = match std::fs::read(&path) {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return BTreeMap::new(),
        Err(e) => {
            tracing::warn!("metadata index unreadable ({e}): {}", path.display());
            return BTreeMap::new();
        }
    };
    match serde_json::from_slice::<Index>(&raw) {
        Ok(idx) => idx.files,
        Err(e) => {
            tracing::warn!("metadata index is not valid JSON ({e}): {}", path.display());
            BTreeMap::new()
        }
    }
}

/// Write the index through a temp file and an atomic rename, so a reader always
/// sees a whole index.
fn save(root: &Path, files: &BTreeMap<String, FileMeta>) -> std::io::Result<()> {
    let temp = root.join(INDEX_TEMP);
    let body = serde_json::to_vec_pretty(&Index { files: files.clone() })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&temp, body)?;
    match std::fs::rename(&temp, root.join(INDEX_NAME)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&temp);
            Err(e)
        }
    }
}

/// Remember where `rows` came from (share-relative path plus origin). Empty
/// metadata is skipped: an unknown origin is stored as no row, not as a row of
/// empty strings. Failures are logged and swallowed, a published file must not
/// be reported as failed because its origin could not be written.
pub fn record(root: &Path, rows: &[(String, FileMeta)]) {
    if rows.iter().all(|(_, m)| m.is_empty()) {
        return;
    }
    let _guard = WRITE_LOCK.lock().expect("metadata lock poisoned");
    let mut files = load(root);
    for (rel, meta) in rows {
        if meta.is_empty() {
            continue;
        }
        files.insert(rel.clone(), meta.clone());
    }
    if let Err(e) = save(root, &files) {
        tracing::warn!("metadata index not written ({e}): {}", root.display());
    }
}

/// Drop the row for `rel`. Called when the agent's own write path replaces or
/// removes a file: whatever lands there next did not come from the old page,
/// and a stale row would tell the consumer it did.
pub fn forget(root: &Path, rel: &str) {
    let _guard = WRITE_LOCK.lock().expect("metadata lock poisoned");
    let mut files = load(root);
    if files.remove(rel).is_none() {
        return;
    }
    if let Err(e) = save(root, &files) {
        tracing::warn!("metadata index not written ({e}): {}", root.display());
    }
}

/// The index key of a file inside a share: its path relative to the root,
/// forward-slash separated, exactly as the manifest names it. `None` for a path
/// that is not under the root at all.
pub fn rel_key(root: &Path, target: &Path) -> Option<String> {
    let rel = target.strip_prefix(root).ok()?;
    Some(
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// Parse a plugin's [`PLUGIN_FILE`]: one line per output file, tab-separated
/// `<file>\t<url>\t<source>\t<source_url>\t<date>\t<title>`, keyed by the file's
/// own name (the plugin may print a path, only the name identifies the output).
/// Junk lines are skipped rather than failing the job.
pub fn parse_plugin_output(text: &str) -> HashMap<String, FileMeta> {
    text.lines().filter_map(parse_line).collect()
}

/// One line of [`PLUGIN_FILE`]. Trailing fields may be missing entirely, so a
/// plugin that only knows the title still says something. A tab inside the last
/// field (the title) is harmless; the split stops before it.
fn parse_line(line: &str) -> Option<(String, FileMeta)> {
    let line = line.trim_end_matches(['\r', '\n']);
    let mut parts = line.splitn(6, '\t');
    let file = file_name_of(parts.next()?.trim());
    if file.is_empty() {
        return None;
    }
    let field = |p: Option<&str>| p.unwrap_or_default().trim().to_string();
    let meta = FileMeta {
        url: field(parts.next()),
        source: field(parts.next()),
        source_url: field(parts.next()),
        published: normalize_date(&field(parts.next())),
        title: field(parts.next()),
    };
    // A line that names a file and says nothing about it is noise, not a row.
    (!meta.is_empty()).then_some((file, meta))
}

/// The file's own name out of whatever the plugin printed (yt-dlp's
/// `%(filepath)s` is a path, and on Windows it is backslash-separated).
fn file_name_of(s: &str) -> String {
    s.rsplit(['/', '\\']).next().unwrap_or(s).to_string()
}

/// `YYYYMMDD` (what yt-dlp gives) becomes `YYYY-MM-DD`; anything else is passed
/// through as the plugin wrote it, including an already dashed date.
fn normalize_date(s: &str) -> String {
    if s.len() == 8 && s.chars().all(|c| c.is_ascii_digit()) {
        format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8])
    } else {
        s.to_string()
    }
}

/// What the file's own name still gives away (XR-255 п. 5). A youtube import
/// carries the video id in a `[...]` suffix, which is enough to rebuild the link
/// to the page. The channel is not in the name and is **not** guessed: the
/// consumer shows honest emptiness instead of an invention.
pub fn from_name(name: &str) -> Option<FileMeta> {
    let stem = match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    };
    let id = youtube_id(stem)?;
    Some(FileMeta {
        url: format!("https://www.youtube.com/watch?v={id}"),
        ..FileMeta::default()
    })
}

/// The trailing `[<id>]` of a yt-dlp output name, exactly 11 base64url
/// characters. The rule is narrow on purpose (the app's `displayFileName` cuts
/// the same suffix): a wider one would read `[2024]` or `[rus]` as an id.
fn youtube_id(stem: &str) -> Option<&str> {
    let inner = stem.strip_suffix(']')?.rsplit_once('[')?.1;
    let ok = inner.len() == 11
        && inner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then_some(inner)
}

/// The one-off pass over a share at agent startup (XR-255 п. 5): fill in what
/// the names of already downloaded files still give away, and drop rows whose
/// file is gone. Never goes to the network and never touches a file that
/// already has a row, so an import's own metadata always wins over a guess.
pub fn backfill(root: &Path) {
    let _guard = WRITE_LOCK.lock().expect("metadata lock poisoned");
    let mut files = load(root);
    let before = files.len();
    let mut present: HashSet<String> = HashSet::new();
    let mut filled = 0usize;

    for entry in crate::manifest::walk_share(root) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(rel) = rel_key(root, entry.path()) else { continue };
        present.insert(rel.clone());
        if files.contains_key(&rel) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(meta) = from_name(&name) {
            files.insert(rel, meta);
            filled += 1;
        }
    }

    files.retain(|rel, _| present.contains(rel));
    let dropped = before + filled - files.len();
    if filled == 0 && dropped == 0 {
        return;
    }
    match save(root, &files) {
        Ok(()) => tracing::info!(
            "share {}: origin filled in for {filled} file(s), {dropped} stale row(s) dropped",
            root.display()
        ),
        Err(e) => tracing::warn!("metadata index not written ({e}): {}", root.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(url: &str, source: &str) -> FileMeta {
        FileMeta { url: url.into(), source: source.into(), ..FileMeta::default() }
    }

    #[test]
    fn index_survives_a_reread() {
        // The whole point of an index on disk: what an import wrote is still
        // there for the next process, keyed by the path the manifest uses.
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty(), "чистая шара без индекса пуста");

        record(dir.path(), &[("видео/Ролик.mp4".into(), meta("https://host/p", "Канал"))]);
        let back = load(dir.path());
        assert_eq!(back.get("видео/Ролик.mp4"), Some(&meta("https://host/p", "Канал")));

        // A second write keeps the first row and adds its own.
        record(dir.path(), &[("второй.mp4".into(), meta("https://host/q", ""))]);
        let back = load(dir.path());
        assert_eq!(back.len(), 2);
        assert_eq!(back["видео/Ролик.mp4"].source, "Канал");

        // Nothing known is no row at all, not a row of empty strings.
        record(dir.path(), &[("пустой.bin".into(), FileMeta::default())]);
        assert!(!load(dir.path()).contains_key("пустой.bin"));

        // The write path forgets a row when it replaces or removes the file.
        forget(dir.path(), "второй.mp4");
        assert!(!load(dir.path()).contains_key("второй.mp4"));
        assert!(load(dir.path()).contains_key("видео/Ролик.mp4"));
    }

    #[test]
    fn corrupt_index_reads_as_empty() {
        // Metadata is a nicety: a half-written or hand-mangled index must not
        // take the listing of the whole share down.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(INDEX_NAME), b"{not json").unwrap();
        assert!(load(dir.path()).is_empty());
        // And a later write repairs it instead of appending to the junk.
        record(dir.path(), &[("a.mp4".into(), meta("https://host/p", ""))]);
        assert_eq!(load(dir.path()).len(), 1);
    }

    #[test]
    fn plugin_output_is_parsed_per_file() {
        let text = "\
/tmp/.xr-import-7/Ролик [dQw4w9WgXcQ].mp4\thttps://www.youtube.com/watch?v=dQw4w9WgXcQ\tКанал\thttps://www.youtube.com/@kanal\t20260731\tНастоящий заголовок
второй.mp4\thttps://host/two\t\t\t\t
мусор без табов
\thttps://host/three
третий.mp4\thttps://host/3\tИсточник";
        let out = parse_plugin_output(text);
        assert_eq!(out.len(), 3, "битые строки пропускаются: {out:?}");

        // The plugin prints a path; the name is what identifies the output.
        let m = &out["Ролик [dQw4w9WgXcQ].mp4"];
        assert_eq!(m.url, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(m.source, "Канал");
        assert_eq!(m.source_url, "https://www.youtube.com/@kanal");
        // yt-dlp's YYYYMMDD is normalized once, on the agent.
        assert_eq!(m.published, "2026-07-31");
        assert_eq!(m.title, "Настоящий заголовок");

        // Empty trailing fields stay empty, missing ones too.
        assert_eq!(out["второй.mp4"], meta("https://host/two", ""));
        assert_eq!(out["третий.mp4"], meta("https://host/3", "Источник"));
    }

    #[test]
    fn name_gives_up_the_youtube_link_and_nothing_else() {
        // XR-255 п. 5: the id in the name rebuilds the page link; the channel
        // is not in the name, so it stays empty rather than invented.
        let m = from_name("Название [dQw4w9WgXcQ].mp4").expect("ссылка собирается");
        assert_eq!(m.url, "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
        assert_eq!(m.source, "");
        assert_eq!(m.title, "");
        // Underscores and dashes are part of the alphabet.
        assert_eq!(
            from_name("Клип [a_b-c1234XY].webm").unwrap().url,
            "https://www.youtube.com/watch?v=a_b-c1234XY"
        );
        // Everything else is not an id: a year, a language tag, a wrong length,
        // a suffix that is not at the end, a name that is all extension.
        for name in [
            "Фильм [2024].mkv",
            "Фильм [rus].mkv",
            "Ролик [dQw4w9WgXc].mp4",
            "Ролик [dQw4w9WgXcQ] (1080p).mp4",
            "обычное имя.txt",
            ".xr-part-abc",
        ] {
            assert!(from_name(name).is_none(), "{name}");
        }
    }

    #[test]
    fn backfill_fills_by_name_and_drops_orphans() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("видео")).unwrap();
        std::fs::write(dir.path().join("видео/Название [dQw4w9WgXcQ].mp4"), b"v").unwrap();
        std::fs::write(dir.path().join("руками.txt"), b"t").unwrap();
        // An import already knows this one, and a guess must not overwrite it.
        std::fs::write(dir.path().join("Ролик [a_b-c1234XY].mp4"), b"v").unwrap();
        record(dir.path(), &[("Ролик [a_b-c1234XY].mp4".into(), meta("https://host/real", "Канал"))]);
        // A row whose file is long gone.
        record(dir.path(), &[("удалённый.mp4".into(), meta("https://host/old", ""))]);

        backfill(dir.path());
        let idx = load(dir.path());

        assert_eq!(
            idx["видео/Название [dQw4w9WgXcQ].mp4"].url,
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        );
        assert!(!idx.contains_key("руками.txt"), "файл руками остаётся без источника");
        assert_eq!(idx["Ролик [a_b-c1234XY].mp4"].source, "Канал", "импорт важнее догадки");
        assert!(!idx.contains_key("удалённый.mp4"), "строка без файла подметается");
    }

    #[test]
    fn backfill_leaves_a_clean_share_alone() {
        // Nothing to fill and nothing stale: no index file appears at all, so a
        // share of hand-uploaded files is not littered with service files.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("руками.txt"), b"t").unwrap();
        backfill(dir.path());
        assert!(!dir.path().join(INDEX_NAME).exists());
    }
}
